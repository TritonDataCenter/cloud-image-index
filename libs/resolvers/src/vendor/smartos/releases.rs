// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! SmartOS release discovery via the Manta-hosted release dirs at
//! `https://us-central.manta.mnx.io/Joyent_Dev/public/SmartOS/`.
//!
//! Each release has a dated directory like `20260430T145637Z/` with
//! a `smartos-<rel>-USB.img.gz` plus `sha256sums.txt`. A sibling
//! `latest` text file at the parent directory holds a Manta path
//! pointing at the current release dir. SmartOS is a rolling
//! release with no streams or majors — so the only tokens we accept
//! are `latest` and an explicit `<YYYYMMDD>T<HHMMSS>Z` timestamp.

use anyhow::{Context, Result};
use regex::Regex;

use crate::verify::parse_sums_file;

const SMARTOS_BASE: &str = "https://us-central.manta.mnx.io/Joyent_Dev/public/SmartOS/";
const LATEST_PATH_PREFIX: &str = "/Joyent_Dev/public/SmartOS/";

#[derive(Debug)]
pub struct Resolved {
    /// Release timestamp (e.g. `20260430T145637Z`). Used as the
    /// manifest version.
    pub release: String,
    pub url: String,
    pub sha256: String,
    /// The vendor document the hash was read from.
    pub checksum_url: String,
    /// The name the image is listed under in that document.
    pub checksum_filename: String,
}

pub async fn resolve(http: &reqwest::Client, release: &str) -> Result<Resolved> {
    let token = release.trim();
    let release_id = if token.eq_ignore_ascii_case("latest") {
        fetch_latest_pointer(http).await?
    } else {
        parse_release(token)?
    };

    let dir = format!("{SMARTOS_BASE}{release_id}/");
    let filename = format!("smartos-{release_id}-USB.img.gz");
    let url = format!("{dir}{filename}");
    let sums_url = format!("{dir}sha256sums.txt");

    eprintln!("Fetching {sums_url}");
    let body = http
        .get(&sums_url)
        .send()
        .await
        .with_context(|| format!("GET {sums_url}"))?
        .error_for_status()
        .with_context(|| format!("status from {sums_url}"))?
        .text()
        .await
        .with_context(|| format!("read body of {sums_url}"))?;
    let sha256 = parse_sums_file(&body, &filename)
        .ok_or_else(|| anyhow::anyhow!("sha256sums at {sums_url} has no entry for {filename}"))?;

    Ok(Resolved {
        release: release_id,
        url,
        sha256,
        checksum_url: sums_url,
        checksum_filename: filename,
    })
}

async fn fetch_latest_pointer(http: &reqwest::Client) -> Result<String> {
    let url = format!("{SMARTOS_BASE}latest");
    eprintln!("Fetching {url}");
    let body = http
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("status from {url}"))?
        .text()
        .await
        .with_context(|| format!("read body of {url}"))?;
    parse_latest_pointer(&body)
        .ok_or_else(|| anyhow::anyhow!("could not parse `latest` pointer at {url}: {body:?}"))
}

/// `latest` is a single-line text file holding a Manta path like
/// `/Joyent_Dev/public/SmartOS/20260430T145637Z`. Strip the prefix
/// and any trailing slash/whitespace to get just the release id.
fn parse_latest_pointer(body: &str) -> Option<String> {
    let line = body.lines().next()?.trim();
    let path = line.trim_end_matches('/');
    let id = path.strip_prefix(LATEST_PATH_PREFIX)?;
    if is_release_id(id) {
        Some(id.to_string())
    } else {
        None
    }
}

fn parse_release(input: &str) -> Result<String> {
    let s = input.trim();
    if !is_release_id(s) {
        anyhow::bail!(
            "smartos: expected `latest` or a release timestamp like \
             `20260430T145637Z`, got {input:?}"
        );
    }
    Ok(s.to_string())
}

/// SmartOS release ids are `<YYYYMMDD>T<HHMMSS>Z`, optionally with a
/// trailing modifier like `_u1` (seen in older releases).
fn is_release_id(s: &str) -> bool {
    let re = match Regex::new(r"^\d{8}T\d{6}Z(?:_[A-Za-z0-9]+)?$") {
        Ok(r) => r,
        Err(_) => return false,
    };
    re.is_match(s)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_latest_pointer_strips_prefix() {
        assert_eq!(
            parse_latest_pointer("/Joyent_Dev/public/SmartOS/20260430T145637Z\n").unwrap(),
            "20260430T145637Z"
        );
        assert_eq!(
            parse_latest_pointer("/Joyent_Dev/public/SmartOS/20260430T145637Z/").unwrap(),
            "20260430T145637Z"
        );
    }

    #[test]
    fn parse_latest_pointer_rejects_unrecognized_payload() {
        assert!(parse_latest_pointer("").is_none());
        assert!(parse_latest_pointer("not a path\n").is_none());
        assert!(parse_latest_pointer("/Joyent_Dev/public/SmartOS/garbage").is_none());
    }

    #[test]
    fn is_release_id_accepts_canonical_and_underscore_variants() {
        assert!(is_release_id("20260430T145637Z"));
        assert!(is_release_id("20110926T021612Z_u1"));
        assert!(!is_release_id("20260430"));
        assert!(!is_release_id(""));
        assert!(!is_release_id("nope"));
        assert!(!is_release_id("20260430T145637Z/"));
    }

    #[test]
    fn parse_release_accepts_valid_ids() {
        assert_eq!(
            parse_release("20260430T145637Z").unwrap(),
            "20260430T145637Z"
        );
        assert_eq!(
            parse_release("  20110926T021612Z_u1  ").unwrap(),
            "20110926T021612Z_u1"
        );
        assert!(parse_release("latest").is_err());
        assert!(parse_release("").is_err());
    }
}
