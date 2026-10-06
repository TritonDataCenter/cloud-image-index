// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! openSUSE Leap release discovery.
//!
//! `download.opensuse.org` runs MirrorCache, which exposes a JSON
//! directory listing via the `?json=1` query — we use it for both
//! the Leap version index and per-version appliance listings.
//!
//! Filename naming changed between Leap 15.x and 16.x:
//!
//! - 15.x: `openSUSE-Leap-<X.Y>-Minimal-VM.x86_64-<X.Y.Z>-Cloud-Build<n>.<m>.qcow2`
//! - 16.x: `Leap-<X.Y>-Minimal-VM.x86_64-Cloud-Build<n>.<m>.qcow2`
//!
//! Both have a sibling `.sha256` sidecar in Linux-style form
//! (`<hex>  <filename>`) and a `.sha256.asc` detached signature.
//! We pick the highest-versioned `Cloud-Build…` qcow2 (skipping
//! the rolling pointer that has no `Build` tag) and pin the hash
//! from the sidecar.

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::vendor::VersionEntry;
use crate::verify::parse_sums_file;

const LEAP_BASE: &str = "https://download.opensuse.org/distribution/leap/";

/// openSUSE's own lifecycle feed: every Leap version with its state
/// (`Stable`, `EOL`, or a pre-release state such as `RC`) and an
/// `upgrade-weight` that orders versions oldest to newest, including the
/// Leap 42.x series that predates 15.0.
const DISTRIBUTIONS_URL: &str = "https://get.opensuse.org/api/v0/distributions.json";

#[derive(Debug, Deserialize)]
pub struct Distributions {
    #[serde(rename = "Leap", default)]
    pub leap: Vec<LeapVersion>,
}

#[derive(Debug, Deserialize)]
pub struct LeapVersion {
    pub version: String,
    pub state: String,
    #[serde(rename = "upgrade-weight")]
    pub upgrade_weight: i64,
}

pub async fn fetch_distributions(http: &reqwest::Client) -> Result<Distributions> {
    eprintln!("Fetching openSUSE distributions.json ...");
    http.get(DISTRIBUTIONS_URL)
        .send()
        .await
        .with_context(|| format!("GET {DISTRIBUTIONS_URL}"))?
        .error_for_status()
        .with_context(|| format!("status from {DISTRIBUTIONS_URL}"))?
        .json::<Distributions>()
        .await
        .with_context(|| format!("parse {DISTRIBUTIONS_URL}"))
}

fn newest_first(d: &Distributions) -> Vec<&LeapVersion> {
    let mut versions: Vec<&LeapVersion> = d.leap.iter().collect();
    versions.sort_by_key(|v| std::cmp::Reverse(v.upgrade_weight));
    versions
}

/// Every Leap version, newest first. Only `Stable` versions are
/// supported; pre-release states are named in the title.
pub fn catalog(d: &Distributions) -> Vec<VersionEntry> {
    newest_first(d)
        .into_iter()
        .map(|v| {
            let title = match v.state.as_str() {
                "Stable" | "EOL" => format!("openSUSE Leap {}", v.version),
                state => format!("openSUSE Leap {} ({state})", v.version),
            };
            VersionEntry {
                token: v.version.clone(),
                series: format!("leap{}", v.version),
                version: v.version.clone(),
                title,
                eol_date: None,
                supported: v.state == "Stable",
                lts: false,
                // A pre-release state (e.g. `RC`) is the development
                // channel.
                dev: match v.state.as_str() {
                    "Stable" | "EOL" => None,
                    state => Some(state.to_lowercase()),
                },
                channel: false,
            }
        })
        .collect()
}

/// The newest `Stable` Leap version.
pub fn latest_stable(d: &Distributions) -> Option<String> {
    newest_first(d)
        .into_iter()
        .find(|v| v.state == "Stable")
        .map(|v| v.version.clone())
}

#[derive(Debug, Deserialize)]
struct DirEntry {
    name: String,
}

#[derive(Debug)]
pub struct Resolved {
    pub leap_version: String,
    /// Build identifier (e.g. `16.0-Build16.2` or `15.6.0-Build19.143`)
    /// — used as the manifest version.
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
    let version = if token.eq_ignore_ascii_case("latest") {
        latest_stable(&fetch_distributions(http).await?)
            .ok_or_else(|| anyhow::anyhow!("no Stable Leap version in {DISTRIBUTIONS_URL}"))?
    } else {
        parse_leap_version(token)?
    };
    find_in_version(http, &version).await
}

async fn find_in_version(http: &reqwest::Client, version: &str) -> Result<Resolved> {
    let appliances_base = format!("{LEAP_BASE}{version}/appliances/");
    let entries = fetch_dir_json(http, &format!("{appliances_base}?json=1")).await?;
    let filename = pick_cloud_build(&entries, version).ok_or_else(|| {
        anyhow::anyhow!("no `Cloud-Build…x86_64.qcow2` entry under {appliances_base}")
    })?;
    let url = format!("{appliances_base}{filename}");
    let sidecar_url = format!("{url}.sha256");
    let sha256 = fetch_sidecar_hash(http, &sidecar_url, &filename).await?;
    let build = build_id(&filename, version).unwrap_or_else(|| filename.clone());

    Ok(Resolved {
        leap_version: version.to_string(),
        build,
        url,
        sha256,
        checksum_url: sidecar_url,
        checksum_filename: filename,
    })
}

async fn fetch_dir_json(http: &reqwest::Client, url: &str) -> Result<Vec<DirEntry>> {
    let body = http
        .get(url)
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
        .ok_or_else(|| anyhow::anyhow!("sha256 at {sidecar_url} has no entry for {filename}"))
}

/// Find a `…Leap-<ver>-Minimal-VM.x86_64-…Cloud-Build<n>.<m>.qcow2`
/// entry, skipping the rolling pointer (no `Build` tag) and any
/// non-Cloud / non-x86_64 flavors.
fn pick_cloud_build(entries: &[DirEntry], leap_version: &str) -> Option<String> {
    let prefix_old = format!("openSUSE-Leap-{leap_version}-Minimal-VM.x86_64-");
    let prefix_new = format!("Leap-{leap_version}-Minimal-VM.x86_64-");
    let mut best: Option<&str> = None;
    for entry in entries {
        let name: &str = entry.name.trim_end_matches('/');
        if !name.ends_with(".qcow2") {
            continue;
        }
        let matches_prefix = name.starts_with(&prefix_old) || name.starts_with(&prefix_new);
        if !matches_prefix {
            continue;
        }
        if !name.contains("-Cloud-Build") {
            continue;
        }
        match best {
            Some(b) if b >= name => {}
            _ => best = Some(name),
        }
    }
    best.map(String::from)
}

/// Extract the build identifier (e.g. `16.0-Build16.2` for Leap 16,
/// `15.6.0-Build19.143` for Leap 15.6). Leap 16 names go straight
/// from the prefix into `Cloud-Build…`, while Leap 15 has an inner
/// version (`15.6.0-Cloud-Build…`); we split on `Cloud-` and stitch
/// whichever side is non-empty back to the leading `<X.Y>`.
fn build_id(filename: &str, leap_version: &str) -> Option<String> {
    let prefix_old = format!("openSUSE-Leap-{leap_version}-Minimal-VM.x86_64-");
    let prefix_new = format!("Leap-{leap_version}-Minimal-VM.x86_64-");
    let inner = filename
        .strip_prefix(&prefix_old)
        .or_else(|| filename.strip_prefix(&prefix_new))?;
    let inner = inner.strip_suffix(".qcow2")?;
    let (head, tail) = inner.split_once("Cloud-")?;
    let head = head.trim_end_matches('-');
    if head.is_empty() {
        Some(format!("{leap_version}-{tail}"))
    } else {
        Some(format!("{head}-{tail}"))
    }
}

fn parse_leap_version(input: &str) -> Result<String> {
    let s = input
        .trim()
        .strip_prefix("Leap-")
        .unwrap_or_else(|| input.trim());
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 2 || parts.iter().any(|p| p.parse::<u32>().is_err()) {
        anyhow::bail!("opensuse: expected a Leap version like '15.6' or '16.0', got {input:?}");
    }
    Ok(s.to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn entries(names: &[&str]) -> Vec<DirEntry> {
        names
            .iter()
            .map(|n| DirEntry {
                name: (*n).to_string(),
            })
            .collect()
    }

    #[test]
    fn pick_cloud_build_handles_leap_16_naming() {
        let dir = entries(&[
            "Leap-16.0-Minimal-VM.x86_64-Cloud-Build16.2.qcow2",
            "Leap-16.0-Minimal-VM.x86_64-Cloud.qcow2",
            "Leap-16.0-Minimal-VM.x86_64-kvm-and-xen-Build16.2.qcow2",
            "Leap-16.0-Minimal-VM.aarch64-Cloud-Build16.2.qcow2",
        ]);
        assert_eq!(
            pick_cloud_build(&dir, "16.0").unwrap(),
            "Leap-16.0-Minimal-VM.x86_64-Cloud-Build16.2.qcow2"
        );
    }

    #[test]
    fn pick_cloud_build_handles_leap_15_naming() {
        let dir = entries(&[
            "openSUSE-Leap-15.6-Minimal-VM.x86_64-15.6.0-Cloud-Build19.143.qcow2",
            "openSUSE-Leap-15.6-Minimal-VM.x86_64-Cloud.qcow2",
            "openSUSE-Leap-15.6-Minimal-VM.x86_64-kvm-and-xen-Build19.143.qcow2",
        ]);
        assert_eq!(
            pick_cloud_build(&dir, "15.6").unwrap(),
            "openSUSE-Leap-15.6-Minimal-VM.x86_64-15.6.0-Cloud-Build19.143.qcow2"
        );
    }

    #[test]
    fn pick_cloud_build_returns_none_when_only_pointer() {
        let dir = entries(&["Leap-16.0-Minimal-VM.x86_64-Cloud.qcow2"]);
        assert!(pick_cloud_build(&dir, "16.0").is_none());
    }

    #[test]
    fn build_id_strips_leap_16_chrome() {
        assert_eq!(
            build_id("Leap-16.0-Minimal-VM.x86_64-Cloud-Build16.2.qcow2", "16.0").unwrap(),
            "16.0-Build16.2"
        );
    }

    #[test]
    fn build_id_strips_leap_15_chrome() {
        assert_eq!(
            build_id(
                "openSUSE-Leap-15.6-Minimal-VM.x86_64-15.6.0-Cloud-Build19.143.qcow2",
                "15.6"
            )
            .unwrap(),
            "15.6.0-Build19.143"
        );
    }

    #[test]
    fn parse_leap_version_accepts_dotted_form() {
        assert_eq!(parse_leap_version("15.6").unwrap(), "15.6");
        assert_eq!(parse_leap_version("Leap-16.0").unwrap(), "16.0");
        assert!(parse_leap_version("15").is_err());
        assert!(parse_leap_version("nine.six").is_err());
        assert!(parse_leap_version("").is_err());
    }

    fn distributions() -> Distributions {
        let json = r#"{
          "Leap": [
            {"name": "openSUSE Leap", "version": "42.3", "state": "EOL", "upgrade-weight": 1},
            {"name": "openSUSE Leap", "version": "16.1", "state": "RC", "upgrade-weight": 11},
            {"name": "openSUSE Leap", "version": "15.6", "state": "Stable", "upgrade-weight": 9},
            {"name": "openSUSE Leap", "version": "16.0", "state": "Stable", "upgrade-weight": 10},
            {"name": "openSUSE Leap", "version": "15.5", "state": "EOL", "upgrade-weight": 8}
          ],
          "Tumbleweed": []
        }"#;
        serde_json::from_str(json).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn catalog_orders_by_upgrade_weight_and_supports_only_stable() {
        let entries = catalog(&distributions());
        let got: Vec<(&str, bool, &str)> = entries
            .iter()
            .map(|e| (e.token.as_str(), e.supported, e.title.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("16.1", false, "openSUSE Leap 16.1 (RC)"),
                ("16.0", true, "openSUSE Leap 16.0"),
                ("15.6", true, "openSUSE Leap 15.6"),
                ("15.5", false, "openSUSE Leap 15.5"),
                ("42.3", false, "openSUSE Leap 42.3"),
            ]
        );
    }

    #[test]
    fn latest_stable_skips_pre_releases_and_the_old_42_numbering() {
        assert_eq!(latest_stable(&distributions()).as_deref(), Some("16.0"));
    }

    #[test]
    fn pre_release_states_are_the_dev_channel() {
        let dev: Vec<(String, Option<String>)> = catalog(&distributions())
            .into_iter()
            .map(|e| (e.token, e.dev))
            .collect();
        assert_eq!(dev[0], ("16.1".to_string(), Some("rc".to_string())));
        assert!(dev[1..].iter().all(|(_, d)| d.is_none()));
    }
}
