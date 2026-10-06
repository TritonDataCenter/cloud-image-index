// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Arch Linux release discovery via the per-build directories at
//! `https://geo.mirror.pkgbuild.com/images/`.
//!
//! Arch publishes one directory per cloud-image build, named
//! `v<YYYYMMDD>.<arch-build-id>/`, plus a `latest/` symlink. Each
//! directory contains:
//!
//! - `Arch-Linux-x86_64-cloudimg-<YYYYMMDD>.<build>.qcow2` — dated
//! - `Arch-Linux-x86_64-cloudimg.qcow2` — rolling pointer
//! - `<file>.SHA256` — Linux-style `<hex>  <filename>` sidecar
//! - `<file>.sig` and `<file>.SHA256.sig` — detached GPG signatures
//!   (TLS-only trust for now; GPG verification is a follow-up)
//!
//! We list `/images/`, pick the highest `v…/` entry
//! (`vYYYYMMDD.NNNNNN`, numbers compared as numbers), then fetch
//! the dated cloudimg's sidecar so the upstream sha256 is known at
//! metadata time.

use anyhow::{Context, Result};
use regex::Regex;

use crate::verify::parse_sums_file;

const IMAGES_BASE: &str = "https://geo.mirror.pkgbuild.com/images/";

#[derive(Debug)]
pub struct Resolved {
    /// Build identifier (e.g. `20260501.523211`) without the leading
    /// `v`. Used as the manifest version.
    pub build: String,
    pub url: String,
    pub sha256: String,
    /// The vendor document the hash was read from.
    pub checksum_url: String,
    /// The name the image is listed under in that document.
    pub checksum_filename: String,
}

pub async fn resolve(http: &reqwest::Client, release: &str) -> Result<Resolved> {
    let token = release.trim();
    let build = if token.eq_ignore_ascii_case("latest") {
        let builds = fetch_builds(http).await?;
        builds
            .into_iter()
            .max()
            .ok_or_else(|| anyhow::anyhow!("no Arch Linux builds found at {IMAGES_BASE}"))?
    } else {
        parse_build(token)?
    };

    let dir = format!("{IMAGES_BASE}v{build}/");
    let filename = format!("Arch-Linux-x86_64-cloudimg-{build}.qcow2");
    let url = format!("{dir}{filename}");
    let sidecar_url = format!("{url}.SHA256");
    let sha256 = fetch_sidecar_hash(http, &sidecar_url, &filename).await?;

    Ok(Resolved {
        build,
        url,
        sha256,
        checksum_url: sidecar_url,
        checksum_filename: filename,
    })
}

async fn fetch_builds(http: &reqwest::Client) -> Result<Vec<String>> {
    eprintln!("Fetching Arch Linux image directory listing ...");
    let body = http
        .get(IMAGES_BASE)
        .send()
        .await
        .with_context(|| format!("GET {IMAGES_BASE}"))?
        .error_for_status()
        .with_context(|| format!("status from {IMAGES_BASE}"))?
        .text()
        .await
        .with_context(|| format!("read body of {IMAGES_BASE}"))?;
    Ok(parse_builds_from_html(&body))
}

fn parse_builds_from_html(body: &str) -> Vec<String> {
    let re = match Regex::new(r#"href="v(\d+\.\d+)/""#) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut builds: Vec<String> = re
        .captures_iter(body)
        .filter_map(|c| Some(c.get(1)?.as_str().to_string()))
        .collect();
    builds.sort_by(|a, b| crate::vendor::natural_cmp(a, b));
    builds.dedup();
    builds
}

/// Accept `v20260501.523211`, `20260501.523211`. The whole string
/// after the optional `v` must match `<digits>.<digits>`.
fn parse_build(input: &str) -> Result<String> {
    let s = input.trim();
    let stripped = s.strip_prefix('v').unwrap_or(s);
    let parts: Vec<&str> = stripped.split('.').collect();
    if parts.len() != 2
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()))
    {
        anyhow::bail!("arch: expected a build like '20260501.523211' or 'latest', got {input:?}");
    }
    Ok(stripped.to_string())
}

async fn fetch_sidecar_hash(
    http: &reqwest::Client,
    sidecar_url: &str,
    filename: &str,
) -> Result<String> {
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
    parse_sums_file(&body, filename)
        .ok_or_else(|| anyhow::anyhow!("sidecar at {sidecar_url} has no entry for {filename}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_builds_extracts_versioned_dirs() {
        let body = r#"
            <a href="latest/">latest/</a>
            <a href="v20260215.491172/">v20260215.491172/</a>
            <a href="v20260501.523211/">v20260501.523211/</a>
            <a href="v20260415.515674/">v20260415.515674/</a>
            <a href="archive/">archive/</a>
        "#;
        let builds = parse_builds_from_html(body);
        assert_eq!(
            builds,
            vec![
                "20260215.491172".to_string(),
                "20260415.515674".to_string(),
                "20260501.523211".to_string(),
            ]
        );
    }

    #[test]
    fn parse_builds_dedupes_and_sorts() {
        let body = r#"href="v20260501.523211/" href="v20260501.523211/" href="v20260215.491172/""#;
        assert_eq!(
            parse_builds_from_html(body),
            vec!["20260215.491172".to_string(), "20260501.523211".to_string()]
        );
    }

    #[test]
    fn parse_builds_orders_build_numbers_numerically() {
        let body = r#"href="v20260501.99999/" href="v20260501.100000/""#;
        assert_eq!(
            parse_builds_from_html(body).last().map(String::as_str),
            Some("20260501.100000")
        );
    }

    #[test]
    fn parse_builds_returns_empty_when_none_present() {
        assert!(parse_builds_from_html("nothing").is_empty());
    }

    #[test]
    fn parse_build_accepts_v_prefix() {
        assert_eq!(parse_build("v20260501.523211").unwrap(), "20260501.523211");
        assert_eq!(parse_build("20260501.523211").unwrap(), "20260501.523211");
        assert_eq!(
            parse_build("  v20260501.523211  ").unwrap(),
            "20260501.523211"
        );
    }

    #[test]
    fn parse_build_rejects_invalid() {
        assert!(parse_build("").is_err());
        assert!(parse_build("rolling").is_err());
        assert!(parse_build("20260501").is_err());
        assert!(parse_build("20260501.523211.0").is_err());
        assert!(parse_build("v.523211").is_err());
    }
}
