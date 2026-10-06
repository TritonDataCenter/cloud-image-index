// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Fedora release discovery via the official `releases.json` feed at
//! `https://fedoraproject.org/releases.json`. Each row is a single
//! artifact (variant × subvariant × arch × format), and the cloud
//! qcow2 we want is `arch=x86_64`, `variant=Cloud`,
//! `subvariant=Cloud_Base`. The same JSON includes the upstream
//! `sha256`, so we get a pinned-hash verifier without a second
//! roundtrip — same shape as Ubuntu Simple Streams.

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::vendor::VersionEntry;

const RELEASES_URL: &str = "https://fedoraproject.org/releases.json";

#[derive(Debug, Deserialize)]
pub struct Entry {
    pub version: String,
    pub arch: String,
    pub link: String,
    pub variant: String,
    pub subvariant: String,
    pub sha256: String,
}

pub async fn fetch(http: &reqwest::Client) -> Result<Vec<Entry>> {
    eprintln!("Fetching Fedora releases.json ...");
    let body = http
        .get(RELEASES_URL)
        .send()
        .await
        .with_context(|| format!("GET {RELEASES_URL}"))?
        .error_for_status()
        .with_context(|| format!("status from {RELEASES_URL}"))?
        .text()
        .await
        .with_context(|| format!("read body of {RELEASES_URL}"))?;
    serde_json::from_str(&body).with_context(|| format!("parse {RELEASES_URL}"))
}

fn is_cloud_base_qcow2(e: &Entry) -> bool {
    e.arch == "x86_64"
        && e.variant == "Cloud"
        && e.subvariant == "Cloud_Base"
        && e.link.ends_with(".qcow2")
}

/// The numeric part of a feed version (`44`, or `45` for `45 Beta`).
fn version_number(version: &str) -> u32 {
    version
        .split(' ')
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

/// Whether a feed version is a pre-release (`45 Beta`) rather than a
/// release (`44`).
fn is_pre_release(version: &str) -> bool {
    !version.chars().all(|c| c.is_ascii_digit())
}

/// The release token for a feed version: Fedora's own path spelling,
/// `45_Beta` for `45 Beta`, so tokens never contain spaces.
pub fn token_for(version: &str) -> String {
    version.replace(' ', "_")
}

/// Every Cloud_Base x86_64 qcow2 version in `releases.json`, newest
/// first (a pre-release before the release it precedes). The feed only
/// carries actively-shipping versions; pre-releases are not supported.
pub fn catalog(entries: &[Entry]) -> Vec<VersionEntry> {
    let mut versions: Vec<&str> = entries
        .iter()
        .filter(|e| is_cloud_base_qcow2(e))
        .map(|e| e.version.as_str())
        .collect();
    versions.sort_by_key(|v| std::cmp::Reverse((version_number(v), is_pre_release(v))));
    versions.dedup();
    versions
        .into_iter()
        .map(|v| {
            let token = token_for(v);
            VersionEntry {
                series: format!("f{token}"),
                version: v.to_string(),
                title: format!("Fedora {v}"),
                eol_date: None,
                supported: !is_pre_release(v),
                lts: false,
                // `45 Beta` -> `beta`
                dev: v.split_once(' ').map(|(_, label)| label.to_lowercase()),
                channel: false,
                token,
            }
        })
        .collect()
}

#[derive(Debug)]
pub struct Resolved {
    /// Fedora major (e.g. `44`). Used as the manifest series.
    pub major: String,
    /// Full build identifier from the filename (e.g. `44-1.7`),
    /// or just the major if the filename can't be parsed.
    pub build: String,
    pub url: String,
    pub sha256: String,
    /// The vendor document the hash was read from.
    pub checksum_url: String,
    /// The name the image is listed under in that document.
    pub checksum_filename: String,
}

/// Extract the `<major>-<build>` segment from a Fedora cloud image
/// filename. Returns `None` for unrecognized shapes; the caller
/// falls back to the bare major in that case.
fn extract_build(link: &str) -> Option<String> {
    let filename = link.rsplit('/').next()?;
    let rest = filename.strip_prefix("Fedora-Cloud-Base-Generic-")?;
    let idx = rest.find(".x86_64.qcow2")?;
    Some(rest[..idx].to_string())
}

/// Resolve a release token to a single Cloud_Base x86_64 qcow2 entry.
/// `latest` picks the numerically-highest release (never a
/// pre-release); explicit tokens
/// like `42` or `f42` pick that version exactly.
pub fn resolve(entries: &[Entry], release: &str) -> Result<Resolved> {
    let cloud: Vec<&Entry> = entries.iter().filter(|e| is_cloud_base_qcow2(e)).collect();
    if cloud.is_empty() {
        anyhow::bail!("no Cloud_Base x86_64 qcow2 entries in {RELEASES_URL}");
    }

    let token = release.trim();
    let target = if token.eq_ignore_ascii_case("latest") {
        cloud
            .iter()
            .filter(|e| !is_pre_release(&e.version))
            .max_by_key(|e| version_number(&e.version))
            .copied()
            .ok_or_else(|| anyhow::anyhow!("no comparable Fedora versions in releases.json"))?
    } else {
        let version = parse_version(token)?;
        cloud
            .iter()
            .find(|e| e.version == version)
            .copied()
            .ok_or_else(|| {
                let available: Vec<&str> = cloud.iter().map(|e| e.version.as_str()).collect();
                anyhow::anyhow!(
                    "fedora: version {version} not found in releases.json (have: {})",
                    available.join(", ")
                )
            })?
    };

    let build = extract_build(&target.link).unwrap_or_else(|| target.version.clone());
    Ok(Resolved {
        major: target.version.clone(),
        build,
        url: target.link.clone(),
        sha256: target.sha256.clone(),
        checksum_url: RELEASES_URL.to_string(),
        checksum_filename: String::new(),
    })
}

/// Accept `42`, `f42`, `Fedora-42` — anything that uniquely
/// identifies the Fedora major — or a pre-release token such as
/// `45_Beta`, returned in the feed's spelling (`45 Beta`).
pub fn parse_version(input: &str) -> Result<String> {
    if let Some((number, label)) = input.trim().split_once('_')
        && !number.is_empty()
        && number.chars().all(|c| c.is_ascii_digit())
        && !label.is_empty()
        && label.chars().all(|c| c.is_ascii_alphanumeric())
    {
        return Ok(format!("{number} {label}"));
    }
    let s = input.trim();
    let stripped = s
        .strip_prefix("Fedora-")
        .or_else(|| s.strip_prefix("fedora-"))
        .or_else(|| s.strip_prefix('f'))
        .or_else(|| s.strip_prefix('F'))
        .unwrap_or(s);
    if stripped.is_empty() || !stripped.chars().all(|c| c.is_ascii_digit()) {
        anyhow::bail!("fedora: expected a version like '42', 'f42', or 'latest', got {input:?}");
    }
    Ok(stripped.to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn entry(version: &str, variant: &str, subvariant: &str, link: &str) -> Entry {
        Entry {
            version: version.to_string(),
            arch: "x86_64".to_string(),
            link: link.to_string(),
            variant: variant.to_string(),
            subvariant: subvariant.to_string(),
            sha256: format!("hash-{version}-{subvariant}"),
        }
    }

    fn sample() -> Vec<Entry> {
        vec![
            entry(
                "42",
                "Cloud",
                "Cloud_Base",
                "https://example.test/Fedora-Cloud-Base-Generic-42-1.1.x86_64.qcow2",
            ),
            entry(
                "42",
                "Cloud",
                "Cloud_Base_UKI",
                "https://example.test/Fedora-Cloud-Base-UEFI-UKI-42-1.1.x86_64.qcow2",
            ),
            entry(
                "43",
                "Cloud",
                "Cloud_Base",
                "https://example.test/Fedora-Cloud-Base-Generic-43-1.6.x86_64.qcow2",
            ),
            entry(
                "44",
                "Cloud",
                "Cloud_Base",
                "https://example.test/Fedora-Cloud-Base-Generic-44-1.7.x86_64.qcow2",
            ),
            // Should be filtered out: Server variant, ISO format, aarch64.
            Entry {
                version: "44".to_string(),
                arch: "aarch64".to_string(),
                link: "https://example.test/aarch64.qcow2".to_string(),
                variant: "Cloud".to_string(),
                subvariant: "Cloud_Base".to_string(),
                sha256: "wrong".to_string(),
            },
        ]
    }

    #[test]
    fn resolve_latest_picks_highest_version() {
        let r = resolve(&sample(), "latest").unwrap();
        assert_eq!(r.major, "44");
        assert_eq!(r.build, "44-1.7");
        assert_eq!(r.sha256, "hash-44-Cloud_Base");
    }

    #[test]
    fn resolve_by_version_finds_match() {
        let r = resolve(&sample(), "42").unwrap();
        assert_eq!(r.major, "42");
        assert_eq!(r.build, "42-1.1");
        // Make sure we didn't pick the UKI subvariant.
        assert!(!r.url.contains("UEFI-UKI"));
    }

    #[test]
    fn extract_build_handles_canonical_shape() {
        assert_eq!(
            extract_build("https://example.test/Fedora-Cloud-Base-Generic-44-1.7.x86_64.qcow2"),
            Some("44-1.7".to_string())
        );
    }

    #[test]
    fn extract_build_returns_none_for_unrecognized_shape() {
        assert_eq!(
            extract_build("https://example.test/Some-Other-Image.qcow2"),
            None
        );
    }

    #[test]
    fn resolve_unknown_version_errors() {
        let err = resolve(&sample(), "99").unwrap_err().to_string();
        assert!(err.contains("99"), "{err}");
    }

    #[test]
    fn parse_version_accepts_common_forms() {
        assert_eq!(parse_version("42").unwrap(), "42");
        assert_eq!(parse_version("f42").unwrap(), "42");
        assert_eq!(parse_version("F42").unwrap(), "42");
        assert_eq!(parse_version("Fedora-42").unwrap(), "42");
        assert_eq!(parse_version("fedora-42").unwrap(), "42");
    }

    #[test]
    fn parse_version_rejects_invalid() {
        assert!(parse_version("").is_err());
        assert!(parse_version("rawhide").is_err());
        assert!(parse_version("42.0").is_err());
        assert!(parse_version("f").is_err());
    }

    fn sample_with_beta() -> Vec<Entry> {
        let mut entries = sample();
        entries.push(entry(
            "45 Beta",
            "Cloud",
            "Cloud_Base",
            "https://example.test/Fedora-Cloud-Base-Generic-45_Beta-1.3.x86_64.qcow2",
        ));
        entries
    }

    #[test]
    fn catalog_lists_pre_releases_first_with_path_safe_tokens_unsupported() {
        let got: Vec<(String, bool, String)> = catalog(&sample_with_beta())
            .into_iter()
            .map(|e| (e.token, e.supported, e.title))
            .collect();
        assert_eq!(
            got,
            vec![
                ("45_Beta".to_string(), false, "Fedora 45 Beta".to_string()),
                ("44".to_string(), true, "Fedora 44".to_string()),
                ("43".to_string(), true, "Fedora 43".to_string()),
                ("42".to_string(), true, "Fedora 42".to_string()),
            ]
        );
    }

    #[test]
    fn resolve_accepts_a_pre_release_token() {
        let r = resolve(&sample_with_beta(), "45_Beta").unwrap();
        assert_eq!(r.major, "45 Beta");
        assert_eq!(r.build, "45_Beta-1.3");
    }

    #[test]
    fn latest_is_never_a_pre_release() {
        let r = resolve(&sample_with_beta(), "latest").unwrap();
        assert_eq!(r.major, "44");
    }

    #[test]
    fn pre_releases_are_the_dev_channel() {
        let dev: Vec<(String, Option<String>)> = catalog(&sample_with_beta())
            .into_iter()
            .map(|e| (e.token, e.dev))
            .collect();
        assert_eq!(dev[0], ("45_Beta".to_string(), Some("beta".to_string())));
        assert!(dev[1..].iter().all(|(_, d)| d.is_none()));
    }
}
