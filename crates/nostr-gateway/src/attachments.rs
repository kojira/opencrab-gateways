//! Local preflight only. Every copy is verified before any upload is started.
use std::path::{Path, PathBuf};

use nostr::Keys;
use opencrab_gate_client::{open_verified, OutboundFile};
use serde_json::Value;

pub(crate) const MAX_FILES: usize = 10;
pub(crate) const MAX_BYTES: u64 = 100 * 1024 * 1024;

pub(crate) struct VerifiedFile {
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub media_type: String,
}

pub(crate) fn descriptors(payload: &Value) -> Result<Vec<OutboundFile>, ()> {
    let value = match payload.get("files") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(value) => value,
    };
    let items = value.as_array().ok_or(())?;
    if items.len() > MAX_FILES {
        return Err(());
    }
    let files = OutboundFile::parse_list(value).map_err(|_| ())?;
    let mut total = 0u64;
    for file in &files {
        total = total.checked_add(file.size).ok_or(())?;
        if file.size > MAX_BYTES || total > MAX_BYTES {
            return Err(());
        }
    }
    Ok(files)
}

pub(crate) fn signer(secret: Option<&str>, self_pubkey: &str) -> Result<Keys, ()> {
    let keys = Keys::parse(secret.ok_or(())?).map_err(|_| ())?;
    if keys.public_key().to_hex() != self_pubkey {
        return Err(());
    }
    Ok(keys)
}

pub(crate) async fn verify_all(
    root: Option<&Path>,
    files: Vec<OutboundFile>,
) -> Result<Vec<VerifiedFile>, ()> {
    let root: PathBuf = root.filter(|p| p.is_absolute()).ok_or(())?.to_owned();
    // This blocking task has no signer, HTTP client, or publisher. Cancellation can
    // leave a local read running, but cannot initiate an external effect.
    tokio::task::spawn_blocking(move || {
        files
            .into_iter()
            .map(|file| {
                let media_type = media_type(&file)?;
                let bytes = open_verified(&root, &file).map_err(|_| ())?;
                Ok(VerifiedFile {
                    bytes,
                    sha256: file.sha256,
                    media_type,
                })
            })
            .collect()
    })
    .await
    .map_err(|_| ())?
}

const SUPPORTED: &[&str] = &[
    "image/png",
    "image/jpg",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "image/svg+xml",
    "video/mp4",
    "video/quicktime",
    "video/mpeg",
    "video/webm",
    "audio/mpeg",
    "audio/mpg",
    "audio/mpeg3",
    "audio/mp3",
    "application/pdf",
    "application/javascript",
    "application/json",
    "application/vnd.ms-fontobject",
    "application/yaml",
    "application/xml",
    "font/otf",
    "font/ttf",
    "font/woff",
    "font/woff2",
    "text/html",
    "text/markdown",
    "text/css",
    "text/x-handlebars-template",
    "text/plain",
    "text/yaml",
    "model/stl",
];

fn media_type(file: &OutboundFile) -> Result<String, ()> {
    if let Some(raw) = &file.media_type {
        if let Ok(parsed) = raw.parse::<mime::Mime>() {
            let essence = parsed.essence_str();
            return if SUPPORTED.contains(&essence) {
                Ok(essence.to_owned())
            } else {
                Err(())
            };
        }
    }
    let extension = Path::new(&file.name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let inferred = match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "mpeg" | "mpg" => "video/mpeg",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "pdf" => "application/pdf",
        "js" => "application/javascript",
        "json" => "application/json",
        "eot" => "application/vnd.ms-fontobject",
        "yaml" | "yml" => "application/yaml",
        "xml" => "application/xml",
        "otf" => "font/otf",
        "ttf" => "font/ttf",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "html" | "htm" => "text/html",
        "md" => "text/markdown",
        "css" => "text/css",
        "hbs" => "text/x-handlebars-template",
        "txt" => "text/plain",
        "stl" => "model/stl",
        _ => return Err(()),
    };
    Ok(inferred.to_owned())
}
