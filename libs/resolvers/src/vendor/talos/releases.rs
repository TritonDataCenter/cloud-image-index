// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Talos release discovery via the GitHub releases API.
//!
//! Talos doesn't publish a Simple-Streams-style metadata feed; the
//! canonical "what is the current stable version" signal is the
//! upstream GitHub release at
//! `https://api.github.com/repos/siderolabs/talos/releases/latest`.
//! We strip the `v` prefix from the `tag_name` field for use in the
//! factory URL.

use anyhow::{Context, Result};
use serde::Deserialize;

const LATEST_URL: &str = "https://api.github.com/repos/siderolabs/talos/releases/latest";

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    #[serde(default)]
    published_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// The newest Talos release: its version (without the `v`) and when
/// GitHub says it was published.
pub struct Latest {
    pub version: String,
    pub published_at: Option<chrono::DateTime<chrono::Utc>>,
}

fn latest_from(release: GitHubRelease) -> Latest {
    Latest {
        version: release
            .tag_name
            .strip_prefix('v')
            .unwrap_or(&release.tag_name)
            .to_string(),
        published_at: release.published_at,
    }
}

pub async fn find_latest(http: &reqwest::Client) -> Result<Latest> {
    eprintln!("Fetching Talos latest release from GitHub ...");
    // GitHub returns 403 to requests without a User-Agent; the caller's
    // client sets one (see the crate documentation).
    let resp = http
        .get(LATEST_URL)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .with_context(|| format!("GET {LATEST_URL}"))?
        .error_for_status()
        .with_context(|| format!("status from {LATEST_URL}"))?
        .json::<GitHubRelease>()
        .await
        .with_context(|| format!("parse {LATEST_URL}"))?;
    Ok(latest_from(resp))
}

/// Validate and normalize a user-supplied version. Accepts `X.Y.Z`
/// and `vX.Y.Z`, returns `X.Y.Z`. Talos uses semver releases.
pub fn parse_version(input: &str) -> Result<String> {
    let s = input.trim();
    let stripped = s.strip_prefix('v').unwrap_or(s);
    let parts: Vec<&str> = stripped.split('.').collect();
    if parts.len() != 3 || parts.iter().any(|p| p.parse::<u32>().is_err()) {
        anyhow::bail!("talos: expected version like '1.12.7' or 'v1.12.7', got {input:?}");
    }
    Ok(stripped.to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_accepts_short_and_v_form() {
        assert_eq!(parse_version("1.12.7").unwrap(), "1.12.7");
        assert_eq!(parse_version("v1.12.7").unwrap(), "1.12.7");
    }

    #[test]
    fn parse_version_rejects_invalid() {
        assert!(parse_version("1.12").is_err());
        assert!(parse_version("1.12.7-rc1").is_err());
        assert!(parse_version("latest").is_err());
        assert!(parse_version("garbage").is_err());
    }

    #[test]
    fn latest_release_reads_version_and_publish_date() {
        let json = r#"{"tag_name": "v1.14.2", "published_at": "2026-09-29T20:44:07Z"}"#;
        let latest = latest_from(serde_json::from_str(json).unwrap());
        assert_eq!(latest.version, "1.14.2");
        assert_eq!(
            latest.published_at.map(|t| t.to_rfc3339()),
            Some("2026-09-29T20:44:07+00:00".to_string())
        );
    }
}
