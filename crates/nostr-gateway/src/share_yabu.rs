//! Target-specific Blossom upload. No discovery, redirects, retries, or deletion.
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use nostr::{EventBuilder, JsonUtil, Keys, Kind, Tag, Timestamp};
use reqwest::{header, Client};
use serde::Deserialize;

use crate::attachments::VerifiedFile;

// The unversioned /upload selects the server's legacy response contract.
const ENDPOINT: &str = "https://share.yabu.me/api/v2/media/upload";
const RESPONSE_LIMIT: usize = 64 * 1024;

pub(crate) struct UploadPolicy {
    endpoint: String,
    request_timeout: Duration,
    pub phase_timeout: Duration,
}

impl Default for UploadPolicy {
    fn default() -> Self {
        Self {
            endpoint: ENDPOINT.into(),
            request_timeout: Duration::from_secs(60),
            phase_timeout: Duration::from_secs(180),
        }
    }
}

impl UploadPolicy {
    pub fn client(&self) -> Result<Client, ()> {
        Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(self.request_timeout)
            .build()
            .map_err(|_| ())
    }

    #[cfg(test)]
    pub fn local(endpoint: String, request_timeout: Duration, phase_timeout: Duration) -> Self {
        assert!(endpoint.starts_with("http://127.0.0.1:"));
        Self {
            endpoint,
            request_timeout,
            phase_timeout,
        }
    }
}

#[derive(Debug)]
pub(crate) enum UploadError {
    Refused,
    Uncertain,
}

// Construct before marking transmission possible; signing errors have no effects.
pub(crate) fn authorization(keys: &Keys, hash: &str) -> Result<String, ()> {
    let now = Timestamp::now();
    let tags = vec![
        Tag::parse(["t", "upload"]).map_err(|_| ())?,
        Tag::parse(["x", hash]).map_err(|_| ())?,
        Tag::parse(["expiration", &(now.as_u64() + 300).to_string()]).map_err(|_| ())?,
    ];
    let event = EventBuilder::new(Kind::Custom(24242), "Upload")
        .custom_created_at(now)
        .tags(tags)
        .sign_with_keys(keys)
        .map_err(|_| ())?;
    // This server's UI uses standard padded encoding (not latest BUD-11).
    Ok(format!(
        "Nostr {}",
        STANDARD.encode(event.as_json().as_bytes())
    ))
}

pub(crate) async fn upload(
    policy: &UploadPolicy,
    client: &Client,
    file: VerifiedFile,
    authorization: String,
) -> Result<String, UploadError> {
    let size = file.bytes.len() as u64;
    let mut response = client
        .put(&policy.endpoint)
        .header(header::AUTHORIZATION, authorization)
        .header(header::CONTENT_TYPE, file.media_type)
        .header(header::CONTENT_LENGTH, size)
        .header("X-SHA-256", &file.sha256)
        .body(file.bytes)
        .send()
        .await
        .map_err(|_| UploadError::Uncertain)?;
    match response.status().as_u16() {
        200 | 201 => {}
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
    struct Descriptor {
        url: String,
        sha256: String,
        size: u64,
    }
    let descriptor: Descriptor =
        serde_json::from_slice(&body).map_err(|_| UploadError::Uncertain)?;
    if descriptor.sha256 != file.sha256 || descriptor.size != size {
        return Err(UploadError::Uncertain);
    }
    let url = url::Url::parse(&descriptor.url).map_err(|_| UploadError::Uncertain)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || descriptor
            .url
            .chars()
            .any(|c| c.is_control() || c.is_whitespace())
        || descriptor.url.split_once("://").is_none_or(|(_, rest)| {
            rest.split(['/', '?', '#'])
                .next()
                .is_none_or(|authority| authority.contains('@'))
        })
    {
        return Err(UploadError::Uncertain);
    }
    Ok(descriptor.url)
}

#[cfg(test)]
mod tests {
    #[test]
    fn default_policy_uses_versioned_blossom_endpoint() {
        assert_eq!(
            super::UploadPolicy::default().endpoint,
            "https://share.yabu.me/api/v2/media/upload"
        );
    }
}
