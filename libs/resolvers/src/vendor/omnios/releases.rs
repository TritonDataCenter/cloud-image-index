// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! OmniOS release discovery.
//!
//! OmniOS publishes one cloud image per release channel at
//! `https://downloads.omnios.org/media/<channel>/`, where channel
//! is one of:
//!
//! - `stable` — current quarterly stable release
//! - `lts` — current LTS (with optional `r` refresh suffix on the version)
//! - `bloody` — bleeding-edge weekly snapshot
//!
//! Each channel directory contains a single `omnios-<id>.cloud.vmdk`
//! and a sibling `<file>.sha256` sidecar that holds a bare SHA-256
//! hash on a single line. Old releases are not archived at
//! predictable URLs; the channel directory is the manifest.
//!
//! `latest` aliases to `stable`. Lex sort is sufficient to pick
//! between concurrent variants in a channel (e.g. lts ships both
//! `r151054` and `r151054r`; the refresh sorts after).

use anyhow::{Context, Result};
use regex::Regex;

const MEDIA_BASE: &str = "https://downloads.omnios.org/media/";

/// Supported channel names (in canonical kebab-case as the release token).
const CHANNELS: &[&str] = &["stable", "lts", "bloody"];

#[derive(Debug)]
pub struct Resolved {
    pub channel: String,
    /// Build identifier (e.g. `r151058`, `r151054r`, `20260319` for
    /// bloody after stripping its `bloody-` prefix). Used as the
    /// manifest version.
    pub build: String,
    pub url: String,
    pub sha256: String,
    /// The checksum sidecar the hash was read from.
    pub checksum_url: String,
}

pub async fn resolve(http: &reqwest::Client, release: &str) -> Result<Resolved> {
    let channel = parse_channel(release)?;
    let dir = format!("{MEDIA_BASE}{channel}/");

    eprintln!("Fetching OmniOS {channel} channel listing ...");
    let body = http
        .get(&dir)
        .send()
        .await
        .with_context(|| format!("GET {dir}"))?
        .error_for_status()
        .with_context(|| format!("status from {dir}"))?
        .text()
        .await
        .with_context(|| format!("read body of {dir}"))?;

    let filename = pick_latest_vmdk(&body)
        .ok_or_else(|| anyhow::anyhow!("no `omnios-*.cloud.vmdk` in {dir}"))?;
    let url = format!("{dir}{filename}");
    let sidecar_url = format!("{url}.sha256");
    let sha256 = fetch_bare_sha256(http, &sidecar_url).await?;

    let build = strip_filename_chrome(&filename, &channel).unwrap_or_else(|| filename.clone());
    Ok(Resolved {
        channel,
        build,
        url,
        sha256,
        checksum_url: sidecar_url,
    })
}

/// Accept the three canonical channel names plus the alias
/// `latest` → `stable`.
fn parse_channel(input: &str) -> Result<String> {
    let s = input.trim().to_lowercase();
    if s == "latest" {
        return Ok("stable".to_string());
    }
    if CHANNELS.contains(&s.as_str()) {
        return Ok(s);
    }
    anyhow::bail!(
        "omnios: expected one of {} (or `latest`), got {input:?}",
        CHANNELS.join(", ")
    );
}

/// Find the highest-versioned `omnios-*.cloud.vmdk` (excluding
/// `.sha256` sidecars). Lex sort works because each channel uses
/// a single naming convention with monotonically-increasing
/// suffixes (`r151058`, `r151054r`, `20260319`).
fn pick_latest_vmdk(body: &str) -> Option<String> {
    let re = Regex::new(r#"href="(omnios-[^"]+?\.cloud\.vmdk)""#).ok()?;
    let mut candidates: Vec<&str> = re
        .captures_iter(body)
        .filter_map(|c| Some(c.get(1)?.as_str()))
        .filter(|n| !n.ends_with(".sha256"))
        .collect();
    candidates.sort();
    candidates.dedup();
    candidates.last().map(|s| s.to_string())
}

/// Strip the channel-specific chrome:
///   - `omnios-r151058.cloud.vmdk` → `r151058`
///   - `omnios-r151054r.cloud.vmdk` → `r151054r`
///   - `omnios-bloody-20260319.cloud.vmdk` → `20260319`
fn strip_filename_chrome(filename: &str, channel: &str) -> Option<String> {
    let stripped = filename.strip_prefix("omnios-")?;
    let stripped = stripped.strip_suffix(".cloud.vmdk")?;
    // For the bloody channel the build identifier is prefixed with
    // the channel name in the filename; trim it to keep manifest
    // version clean.
    let bloody_prefix = format!("{channel}-");
    Some(
        stripped
            .strip_prefix(&bloody_prefix)
            .unwrap_or(stripped)
            .to_string(),
    )
}

async fn fetch_bare_sha256(http: &reqwest::Client, sidecar_url: &str) -> Result<String> {
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
    let token = body
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty sidecar at {sidecar_url}"))?
        .to_lowercase();
    if token.len() != 64 || !token.chars().all(|c| c.is_ascii_hexdigit()) {
        anyhow::bail!("sidecar at {sidecar_url} is not a 64-char hex sha256: {token:?}");
    }
    Ok(token)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_channel_accepts_canonical_names_and_latest() {
        assert_eq!(parse_channel("stable").unwrap(), "stable");
        assert_eq!(parse_channel("LTS").unwrap(), "lts");
        assert_eq!(parse_channel("  bloody  ").unwrap(), "bloody");
        assert_eq!(parse_channel("latest").unwrap(), "stable");
        assert!(parse_channel("").is_err());
        assert!(parse_channel("rolling").is_err());
        assert!(parse_channel("r151058").is_err());
    }

    #[test]
    fn pick_latest_vmdk_skips_sha256_sidecars() {
        let body = r#"
            <a href="omnios-r151054.cloud.vmdk">omnios-r151054.cloud.vmdk</a>
            <a href="omnios-r151054.cloud.vmdk.sha256">…</a>
            <a href="omnios-r151054r.cloud.vmdk">omnios-r151054r.cloud.vmdk</a>
            <a href="omnios-r151054r.cloud.vmdk.sha256">…</a>
        "#;
        assert_eq!(
            pick_latest_vmdk(body).unwrap(),
            "omnios-r151054r.cloud.vmdk"
        );
    }

    #[test]
    fn pick_latest_vmdk_returns_none_when_only_sidecars() {
        let body = r#"<a href="omnios-r151054.cloud.vmdk.sha256">…</a>"#;
        assert!(pick_latest_vmdk(body).is_none());
    }

    #[test]
    fn strip_filename_chrome_handles_stable_lts_bloody() {
        assert_eq!(
            strip_filename_chrome("omnios-r151058.cloud.vmdk", "stable").unwrap(),
            "r151058"
        );
        assert_eq!(
            strip_filename_chrome("omnios-r151054r.cloud.vmdk", "lts").unwrap(),
            "r151054r"
        );
        assert_eq!(
            strip_filename_chrome("omnios-bloody-20260319.cloud.vmdk", "bloody").unwrap(),
            "20260319"
        );
        assert_eq!(
            strip_filename_chrome("Some-Other-File.qcow2", "stable"),
            None
        );
    }
}
