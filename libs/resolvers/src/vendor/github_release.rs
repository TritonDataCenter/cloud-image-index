// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Shared helpers for images published as GitHub release assets with a
//! `<asset>.sha256` sidecar beside them. Used by openbsd and nixos
//! release discovery.
//!
//! Trust roots in TLS to `api.github.com` / `github.com`. GitHub
//! requires a User-Agent on API requests; the caller's client must set
//! one (see the crate documentation).

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub published_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
}

/// A release asset and the hash its `.sha256` sidecar gives for it.
#[derive(Debug)]
pub struct PinnedAsset {
    pub url: String,
    pub sha256: String,
    /// The checksum sidecar asset the hash was read from.
    pub checksum_url: String,
}

/// GET a GitHub API URL and parse the JSON body.
pub async fn get<T: for<'de> Deserialize<'de>>(http: &reqwest::Client, url: &str) -> Result<T> {
    eprintln!("Fetching {url}");
    let body = http
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("status from {url}"))?
        .text()
        .await
        .with_context(|| format!("read body of {url}"))?;
    serde_json::from_str(&body).with_context(|| format!("parse {url}"))
}

/// The asset named `name` in `release`, pinned to the hash in its
/// `<name>.sha256` sidecar asset.
pub async fn pinned_asset(
    http: &reqwest::Client,
    release: &Release,
    name: &str,
) -> Result<PinnedAsset> {
    let find = |wanted: &str| {
        release
            .assets
            .iter()
            .find(|a| a.name == wanted)
            .ok_or_else(|| anyhow::anyhow!("release {} has no `{wanted}` asset", release.tag_name))
    };
    let asset = find(name)?;
    let sidecar = find(&format!("{name}.sha256"))?;
    let sha256 = fetch_first_hex_hash(http, &sidecar.browser_download_url).await?;
    Ok(PinnedAsset {
        url: asset.browser_download_url.clone(),
        sha256,
        checksum_url: sidecar.browser_download_url.clone(),
    })
}

async fn fetch_first_hex_hash(http: &reqwest::Client, sidecar_url: &str) -> Result<String> {
    eprintln!("Fetching {sidecar_url}");
    let body = http
        .get(sidecar_url)
        .send()
        .await
        .with_context(|| format!("GET {sidecar_url}"))?
        .error_for_status()
        .with_context(|| format!("status from {sidecar_url}"))?
        .text()
        .await
        .with_context(|| format!("read body of {sidecar_url}"))?;
    parse_first_hex64(&body)
        .ok_or_else(|| anyhow::anyhow!("no 64-char hex token in sidecar at {sidecar_url}"))
}

/// Pull the first whitespace-delimited 64-char lowercase hex token from
/// a sidecar body. Tolerates BSD-style `SHA256 (file) = hex`,
/// Linux-style `<hex>  <filename>`, and bare-hash sidecars.
fn parse_first_hex64(body: &str) -> Option<String> {
    body.split_whitespace().find_map(|tok| {
        let tok = tok.trim().trim_end_matches(',').to_lowercase();
        if tok.len() == 64 && tok.chars().all(|c| c.is_ascii_hexdigit()) {
            Some(tok)
        } else {
            None
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_first_hex64_handles_linux_style_with_path() {
        let body = "055ebb5f1ef22e6556a94efc735feedd6404155bfdfc4b1803d7fb67cfd7fab5  images/openbsd-min.qcow2\n";
        assert_eq!(
            parse_first_hex64(body).unwrap(),
            "055ebb5f1ef22e6556a94efc735feedd6404155bfdfc4b1803d7fb67cfd7fab5"
        );
    }

    #[test]
    fn parse_first_hex64_handles_bare_hash() {
        let body = "  abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789  \n";
        assert_eq!(
            parse_first_hex64(body).unwrap(),
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
        );
    }

    #[test]
    fn parse_first_hex64_returns_none_when_no_match() {
        assert!(parse_first_hex64("not a hash\n").is_none());
        // Wrong length.
        assert!(parse_first_hex64("abc123  file\n").is_none());
    }

    #[test]
    fn release_payload_carries_publish_date() {
        let json = r#"{"tag_name": "v7.9_2026-08-31-22-45", "published_at": "2026-08-31T23:00:00Z", "assets": []}"#;
        let r: Release = serde_json::from_str(json).unwrap();
        assert_eq!(
            r.published_at.map(|t| t.to_rfc3339()),
            Some("2026-08-31T23:00:00+00:00".to_string())
        );
    }
}
