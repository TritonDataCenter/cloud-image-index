// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! CentOS Stream release discovery.
//!
//! Streams are listed at `https://cloud.centos.org/centos/` as
//! `<n>-stream/` directories (currently 8/9/10; 8 is EOL but still
//! present). Each stream's images live at
//! `<base><n>-stream/x86_64/images/`, with dated builds:
//!
//! - `CentOS-Stream-GenericCloud-<n>-<date>.<build>.x86_64.qcow2`
//! - `<file>.SHA256SUM` — BSD-traditional `SHA256 (filename) = hex`
//! - rolling `<n>-latest.x86_64.qcow2` pointer alongside
//!
//! We list the directory, pick the highest dated build by lex sort
//! (the `<YYYYMMDD>.<n>` shape sorts correctly), then fetch the
//! sidecar at resolve time so the upstream sha256 is known at
//! metadata time.

use anyhow::{Context, Result};

use crate::vendor::dirlist;
use crate::verify::parse_bsd_sums_file;

const STREAMS_BASE: &str = "https://cloud.centos.org/centos/";
const STREAM_DIR_RE: &str = r#"href="(\d+)-stream/""#;

#[derive(Debug)]
pub struct Resolved {
    pub stream: String,
    /// Filename minus the `CentOS-Stream-GenericCloud-<n>-` prefix
    /// and `.x86_64.qcow2` suffix (e.g. `20260504.0`). Used as the
    /// manifest version.
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
    let stream = if token.eq_ignore_ascii_case("latest") {
        let streams = dirlist::fetch_numeric_subdirs(http, STREAMS_BASE, STREAM_DIR_RE).await?;
        streams
            .iter()
            .max()
            .copied()
            .ok_or_else(|| anyhow::anyhow!("no CentOS streams found at {STREAMS_BASE}"))?
            .to_string()
    } else {
        parse_stream(token)?
    };

    let images_base = format!("https://cloud.centos.org/centos/{stream}-stream/x86_64/images/");
    eprintln!("Fetching CentOS Stream {stream} image listing ...");
    let body = http
        .get(&images_base)
        .send()
        .await
        .with_context(|| format!("GET {images_base}"))?
        .error_for_status()
        .with_context(|| format!("status from {images_base}"))?
        .text()
        .await
        .with_context(|| format!("read body of {images_base}"))?;

    let filename = find_latest_dated(&body, &stream).ok_or_else(|| {
        anyhow::anyhow!("no dated `CentOS-Stream-GenericCloud-{stream}-…` qcow2 in {images_base}")
    })?;
    let build = strip_filename_chrome(&filename, &stream).unwrap_or_else(|| filename.clone());

    let url = format!("{images_base}{filename}");
    let sidecar_url = format!("{url}.SHA256SUM");
    let sha256 = fetch_sidecar_hash(http, &sidecar_url, &filename).await?;

    Ok(Resolved {
        stream,
        build,
        url,
        sha256,
        checksum_url: sidecar_url,
        checksum_filename: filename,
    })
}

/// List the published streams (e.g. `[8, 9, 10]`) from the same parent
/// index the resolver enumerates for `latest`. The CloudFront-fronted
/// host needs a non-empty User-Agent, which the caller's client sets.
pub async fn list(http: &reqwest::Client) -> Result<Vec<u32>> {
    dirlist::fetch_numeric_subdirs(http, STREAMS_BASE, STREAM_DIR_RE).await
}

fn parse_stream(input: &str) -> Result<String> {
    let s = input.trim();
    let stripped = s.strip_suffix("-stream").unwrap_or(s);
    if stripped.is_empty() || !stripped.chars().all(|c| c.is_ascii_digit()) {
        anyhow::bail!("centosstream: expected a stream like '9' or 'latest', got {input:?}");
    }
    Ok(stripped.to_string())
}

/// Lex-sort works because all candidates share the same stream
/// prefix and the build suffix is `<YYYYMMDD>.<n>`.
fn find_latest_dated(body: &str, stream: &str) -> Option<String> {
    let prefix = format!("CentOS-Stream-GenericCloud-{stream}-");
    let suffix = ".x86_64.qcow2";
    let mut candidates: Vec<&str> = body
        .split(['"', '<', '>', ' ', '\n'])
        .filter(|name| {
            name.starts_with(&prefix)
                && name.ends_with(suffix)
                && !name.ends_with(".SHA256SUM")
                && !name.ends_with(".SHA1SUM")
                && !name.ends_with(".MD5SUM")
        })
        .filter(|name| {
            // Skip the rolling pointer `…-<stream>-latest.x86_64.qcow2`.
            let middle = &name[prefix.len()..name.len() - suffix.len()];
            middle != "latest"
        })
        .collect();
    candidates.sort();
    candidates.dedup();
    candidates.last().map(|s| s.to_string())
}

fn strip_filename_chrome(filename: &str, stream: &str) -> Option<String> {
    let prefix = format!("CentOS-Stream-GenericCloud-{stream}-");
    let stripped = filename.strip_prefix(&prefix)?;
    let stripped = stripped.strip_suffix(".x86_64.qcow2")?;
    Some(stripped.to_string())
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
    parse_bsd_sums_file(&body, filename)
        .ok_or_else(|| anyhow::anyhow!("SHA256SUM at {sidecar_url} has no entry for {filename}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_stream_accepts_bare_or_suffixed() {
        assert_eq!(parse_stream("9").unwrap(), "9");
        assert_eq!(parse_stream("9-stream").unwrap(), "9");
        assert_eq!(parse_stream("10-stream").unwrap(), "10");
        assert!(parse_stream("9.5").is_err());
        assert!(parse_stream("nine").is_err());
        assert!(parse_stream("").is_err());
    }

    #[test]
    fn find_latest_dated_picks_highest_build() {
        let body = r#"
            <a href="CentOS-Stream-GenericCloud-9-20260302.0.x86_64.qcow2">…</a>
            <a href="CentOS-Stream-GenericCloud-9-20260413.0.x86_64.qcow2">…</a>
            <a href="CentOS-Stream-GenericCloud-9-20260504.0.x86_64.qcow2">…</a>
            <a href="CentOS-Stream-GenericCloud-9-latest.x86_64.qcow2">…</a>
            <a href="CentOS-Stream-GenericCloud-9-20260504.0.x86_64.qcow2.SHA256SUM">…</a>
        "#;
        assert_eq!(
            find_latest_dated(body, "9").unwrap(),
            "CentOS-Stream-GenericCloud-9-20260504.0.x86_64.qcow2"
        );
    }

    #[test]
    fn find_latest_dated_returns_none_when_only_pointer_present() {
        let body = "CentOS-Stream-GenericCloud-9-latest.x86_64.qcow2";
        assert!(find_latest_dated(body, "9").is_none());
    }

    #[test]
    fn strip_chrome_extracts_build() {
        assert_eq!(
            strip_filename_chrome("CentOS-Stream-GenericCloud-9-20260504.0.x86_64.qcow2", "9"),
            Some("20260504.0".to_string())
        );
        assert_eq!(strip_filename_chrome("Some-Other-File.qcow2", "9"), None);
    }
}
