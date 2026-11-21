// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! OpenBSD cloud-image release discovery.
//!
//! Images are published as GitHub release assets at
//! `hcartiaux/openbsd-cloud-image`, with two flavors per release:
//!
//! - `openbsd-min.qcow2` — slim cloud image (no X11 / extras)
//! - `openbsd-generic.qcow2` — full GENERIC kernel build
//!
//! Each has a sibling `<file>.sha256` asset. We always pick the
//! `min` variant — it's the cloud-init-NoCloud target documented at
//! bsd-cloud-image.org. Tags look like `v7.8_2025-10-22-09-25`; we
//! parse the leading `v<X>.<Y>` to expose a friendly `release` token.
//!
//! Trust roots in TLS to `api.github.com` / `github.com`. The
//! sidecar contains a Linux-style `<hex>  images/openbsd-min.qcow2`
//! line with the hash; we take the first hex64 token, since the
//! filename in the sidecar (`images/…`) doesn't match the asset
//! name (`…`) — same "single-hash sidecar" pattern Alpine uses.

use anyhow::Result;

use super::super::github_release::{self, Release};

const REPO: &str = "hcartiaux/openbsd-cloud-image";
const ASSET_NAME: &str = "openbsd-min.qcow2";

#[derive(Debug)]
pub struct Resolved {
    /// Friendly release version (e.g. `7.8`).
    pub version: String,
    /// Full upstream tag (e.g. `v7.8_2025-10-22-09-25`).
    pub tag: String,
    pub url: String,
    pub sha256: String,
    /// The checksum sidecar asset the hash was read from.
    pub checksum_url: String,
    /// When GitHub says the release was published.
    pub published_at: Option<chrono::DateTime<chrono::Utc>>,
}

pub async fn resolve(http: &reqwest::Client, release: &str) -> Result<Resolved> {
    let release_payload = fetch_release(http, release).await?;
    let asset = github_release::pinned_asset(http, &release_payload, ASSET_NAME).await?;

    Ok(Resolved {
        version: parse_version_from_tag(&release_payload.tag_name)
            .unwrap_or_else(|| release_payload.tag_name.clone()),
        tag: release_payload.tag_name.clone(),
        url: asset.url,
        sha256: asset.sha256,
        checksum_url: asset.checksum_url,
        published_at: release_payload.published_at,
    })
}

async fn fetch_release(http: &reqwest::Client, release: &str) -> Result<Release> {
    let token = release.trim();
    if token.eq_ignore_ascii_case("latest") {
        let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
        return github_release::get(http, &url).await;
    }

    let target = parse_version_input(token)?;
    let url = format!("https://api.github.com/repos/{REPO}/releases?per_page=100");
    let releases: Vec<Release> = github_release::get(http, &url).await?;
    releases
        .into_iter()
        .find(|r| {
            parse_version_from_tag(&r.tag_name)
                .map(|v| v == target)
                .unwrap_or(false)
        })
        .ok_or_else(|| anyhow::anyhow!("openbsd: no release matching version {target}"))
}

/// Tags look like `v7.8_2025-10-22-09-25`. Pull just the `<X>.<Y>`
/// after the leading `v`.
fn parse_version_from_tag(tag: &str) -> Option<String> {
    let stripped = tag.strip_prefix('v')?;
    let head = stripped.split('_').next()?;
    let parts: Vec<&str> = head.split('.').collect();
    if parts.len() == 2 && parts.iter().all(|p| p.parse::<u32>().is_ok()) {
        Some(head.to_string())
    } else {
        None
    }
}

/// Accept `7.8`, `v7.8`, or the full tag form for explicit pinning.
fn parse_version_input(input: &str) -> Result<String> {
    let s = input
        .trim()
        .strip_prefix('v')
        .unwrap_or_else(|| input.trim());
    let head = s.split('_').next().unwrap_or(s);
    let parts: Vec<&str> = head.split('.').collect();
    if parts.len() != 2 || parts.iter().any(|p| p.parse::<u32>().is_err()) {
        anyhow::bail!("openbsd: expected a version like '7.8', 'v7.8', or 'latest', got {input:?}");
    }
    Ok(head.to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_from_tag_extracts_dotted_pair() {
        assert_eq!(
            parse_version_from_tag("v7.8_2025-10-22-09-25").unwrap(),
            "7.8"
        );
        assert_eq!(parse_version_from_tag("v7.7_anything").unwrap(), "7.7");
        assert!(parse_version_from_tag("7.8").is_none());
        assert!(parse_version_from_tag("vlatest").is_none());
    }

    #[test]
    fn parse_version_input_accepts_common_forms() {
        assert_eq!(parse_version_input("7.8").unwrap(), "7.8");
        assert_eq!(parse_version_input("v7.8").unwrap(), "7.8");
        assert_eq!(parse_version_input("v7.8_2025-10-22-09-25").unwrap(), "7.8");
        assert!(parse_version_input("").is_err());
        assert!(parse_version_input("7").is_err());
        assert!(parse_version_input("seven.eight").is_err());
    }
}
