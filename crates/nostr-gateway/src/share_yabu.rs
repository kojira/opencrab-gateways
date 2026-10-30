//! Target-specific, original-preserving upload. No redirects, retries, or deletion.
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use nostr::{EventBuilder, JsonUtil, Keys, Kind, Tag, Timestamp};
use reqwest::{header, Client};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::attachments::VerifiedFile;

// Explicit no_transform is a multipart field, not a Blossom PUT option.
const ENDPOINT: &str = "https://share.yabu.me/api/v2/media";
const RESPONSE_LIMIT: usize = 64 * 1024;

pub(crate) struct UploadPolicy {
    endpoint: String,
    request_timeout: Duration,
    pub phase_timeout: Duration,
    /// Tests talk to a 127.0.0.1 server. Looking up the system proxy (macOS can block for
    /// ~25 s on the first lookup in a process) would eat the tests' short deadlines.
    skip_system_proxy: bool,
}

impl Default for UploadPolicy {
    fn default() -> Self {
        Self {
            endpoint: ENDPOINT.into(),
            request_timeout: Duration::from_secs(60),
            phase_timeout: Duration::from_secs(180),
            skip_system_proxy: false,
        }
    }
}

impl UploadPolicy {
    pub fn client(&self) -> Result<Client, ()> {
        let mut builder = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(self.request_timeout);
        if self.skip_system_proxy {
            builder = builder.no_proxy();
        }
        builder.build().map_err(|_| ())
    }

    #[cfg(test)]
    pub fn local(endpoint: String, request_timeout: Duration, phase_timeout: Duration) -> Self {
        assert!(endpoint.starts_with("http://127.0.0.1:"));
        Self {
            endpoint,
            request_timeout,
            phase_timeout,
            skip_system_proxy: true,
        }
    }

    fn accepts_scheme(&self, url: &url::Url) -> bool {
        #[cfg(test)]
        if url.scheme() == "http"
            && url.origin() == url::Url::parse(&self.endpoint).unwrap().origin()
        {
            return true;
        }
        url.scheme() == "https"
    }
}

#[derive(Debug)]
pub(crate) enum UploadError {
    Refused,
    Uncertain,
}

pub(crate) struct PreparedUpload {
    body: Vec<u8>,
    content_type: String,
    authorization: String,
    sha256: String,
    size: u64,
}

// Encode once and sign the exact wire body before marking transmission possible.
pub(crate) fn prepare(
    policy: &UploadPolicy,
    keys: &Keys,
    file: VerifiedFile,
) -> Result<PreparedUpload, ()> {
    let size = file.bytes.len() as u64;
    let boundary = format!("oc-{}", file.sha256);
    let mut body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"no_transform\"\r\n\r\ntrue\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"attachment\"\r\nContent-Type: {}\r\n\r\n",
        file.media_type
    ).into_bytes();
    body.extend_from_slice(&file.bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let payload_hash = format!("{:x}", Sha256::digest(&body));
    let tags = vec![
        Tag::parse(["u", &policy.endpoint]).map_err(|_| ())?,
        Tag::parse(["method", "POST"]).map_err(|_| ())?,
        Tag::parse(["payload", &payload_hash]).map_err(|_| ())?,
    ];
    let event = EventBuilder::new(Kind::Custom(27235), "")
        .custom_created_at(Timestamp::now())
        .tags(tags)
        .sign_with_keys(keys)
        .map_err(|_| ())?;
    Ok(PreparedUpload {
        body,
        content_type: format!("multipart/form-data; boundary={boundary}"),
        authorization: format!("Nostr {}", STANDARD.encode(event.as_json().as_bytes())),
        sha256: file.sha256,
        size,
    })
}

pub(crate) async fn upload(
    policy: &UploadPolicy,
    client: &Client,
    prepared: PreparedUpload,
) -> Result<String, UploadError> {
    let mut response = client
        .post(&policy.endpoint)
        .header(header::AUTHORIZATION, prepared.authorization)
        .header(header::CONTENT_TYPE, prepared.content_type)
        .header(header::CONTENT_LENGTH, prepared.body.len())
        .body(prepared.body)
        .send()
        .await
        .map_err(|_| UploadError::Uncertain)?;
    match response.status().as_u16() {
        200..=202 => {}
        401 | 402 | 403 | 413 | 415 | 429 => return Err(UploadError::Refused),
        _ => return Err(UploadError::Uncertain),
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| UploadError::Uncertain)? {
        if chunk.len() > RESPONSE_LIMIT - body.len() {
            return Err(UploadError::Uncertain);
        }
        body.extend_from_slice(&chunk);
    }
    #[derive(Deserialize)]
    struct Nip94 {
        tags: Vec<Vec<String>>,
    }
    #[derive(Deserialize)]
    struct Descriptor {
        status: String,
        processing_url: String,
        nip94_event: Nip94,
    }
    let descriptor: Descriptor =
        serde_json::from_slice(&body).map_err(|_| UploadError::Uncertain)?;
    if descriptor.status != "success" || !descriptor.processing_url.is_empty() {
        return Err(UploadError::Uncertain);
    }
    let tag = |name: &str| -> Result<&str, UploadError> {
        let mut matching = descriptor
            .nip94_event
            .tags
            .iter()
            .filter(|t| t.first().is_some_and(|v| v == name));
        let value = matching
            .next()
            .and_then(|t| t.get(1))
            .ok_or(UploadError::Uncertain)?;
        if matching.next().is_some() {
            return Err(UploadError::Uncertain);
        }
        Ok(value)
    };
    if tag("x")? != prepared.sha256
        || tag("ox")? != prepared.sha256
        || tag("size")?.parse::<u64>().ok() != Some(prepared.size)
        || tag("no_transform")? != "true"
    {
        return Err(UploadError::Uncertain);
    }
    let returned_url = tag("url")?;
    let url = url::Url::parse(returned_url).map_err(|_| UploadError::Uncertain)?;
    if !policy.accepts_scheme(&url)
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || returned_url
            .chars()
            .any(|c| c.is_control() || c.is_whitespace())
        || returned_url.split_once("://").is_none_or(|(_, rest)| {
            rest.split(['/', '?', '#'])
                .next()
                .is_none_or(|authority| authority.contains('@'))
        })
    {
        return Err(UploadError::Uncertain);
    }
    // A queued descriptor can describe original bytes while the URL serves a
    // conversion. Publish only after one bounded, unauthenticated GET verifies it.
    let mut download = client
        .get(url)
        .send()
        .await
        .map_err(|_| UploadError::Uncertain)?;
    if download.status().as_u16() != 200
        || download.content_length().is_some_and(|n| n > prepared.size)
    {
        return Err(UploadError::Uncertain);
    }
    let mut received = 0u64;
    let mut digest = Sha256::new();
    while let Some(chunk) = download.chunk().await.map_err(|_| UploadError::Uncertain)? {
        if chunk.len() as u64 > prepared.size - received {
            return Err(UploadError::Uncertain);
        }
        received += chunk.len() as u64;
        digest.update(&chunk);
    }
    if received != prepared.size || format!("{:x}", digest.finalize()) != prepared.sha256 {
        return Err(UploadError::Uncertain);
    }
    Ok(returned_url.to_owned())
}

#[cfg(test)]
mod tests {
    #[test]
    fn default_policy_uses_versioned_media_endpoint() {
        assert_eq!(
            super::UploadPolicy::default().endpoint,
            "https://share.yabu.me/api/v2/media"
        );
    }
}
