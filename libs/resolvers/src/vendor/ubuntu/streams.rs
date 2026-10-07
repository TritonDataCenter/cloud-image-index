// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Canonical's Simple Streams metadata feed for released cloud images.
//!
//! The feed is the same one consumed by `cloud-init`, MAAS, and
//! OpenStack image importers. Each product is keyed by
//! `com.ubuntu.cloud:server:<version>:<arch>`; each product has a map
//! of date-stamped serials, each serial has a map of items keyed by
//! filetype (`disk1.img`, `tar.gz`, `vmdk`, ...). We pick `disk1.img`
//! for amd64 — that's the qcow2 NoCloud-capable cloud image.
//!
//! Resolution paths:
//! - `latest` → newest supported LTS (highest `version`).
//! - codename (`noble`) → product whose `release` matches.
//! - version (`24.04`) → product whose `version` matches.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::Deserialize;
use url::Url;

use crate::vendor::VersionEntry;

pub const STREAMS_URL: &str =
    "https://cloud-images.ubuntu.com/releases/streams/v1/com.ubuntu.cloud:released:download.json";
const BASE_URL: &str = "https://cloud-images.ubuntu.com/";

#[derive(Deserialize)]
pub struct Streams {
    pub products: BTreeMap<String, Product>,
}

#[derive(Deserialize)]
pub struct Product {
    pub arch: String,
    pub release: String,
    #[serde(default)]
    pub release_title: String,
    #[serde(default)]
    pub supported: bool,
    pub version: String,
    pub versions: BTreeMap<String, Version>,
    /// End of standard support (`YYYY-MM-DD`), when the feed gives it.
    #[serde(default)]
    pub support_eol: Option<String>,
}

#[derive(Deserialize)]
pub struct Version {
    pub items: BTreeMap<String, Item>,
}

#[derive(Deserialize)]
pub struct Item {
    pub path: String,
    pub sha256: Option<String>,
}

pub struct StreamsImage {
    pub codename: String,
    pub release_title: String,
    pub serial: String,
    pub url: Url,
    pub sha256: String,
}

pub async fn fetch(http: &reqwest::Client) -> Result<Streams> {
    eprintln!("Fetching Ubuntu Simple Streams index ...");
    let resp = http
        .get(STREAMS_URL)
        .send()
        .await
        .with_context(|| format!("GET {STREAMS_URL}"))?
        .error_for_status()
        .with_context(|| format!("status from {STREAMS_URL}"))?;
    resp.json::<Streams>()
        .await
        .with_context(|| format!("parse {STREAMS_URL}"))
}

pub fn resolve(streams: &Streams, token: &str) -> Result<StreamsImage> {
    let token = token.trim();
    let amd64: Vec<&Product> = streams
        .products
        .values()
        .filter(|p| p.arch == "amd64")
        .collect();

    let product = if token == "latest" {
        amd64
            .iter()
            .copied()
            .filter(|p| p.supported && is_lts(&p.release_title))
            .max_by(|a, b| a.version.cmp(&b.version))
            .ok_or_else(|| anyhow::anyhow!("no supported LTS found in Ubuntu streams"))?
    } else {
        amd64
            .iter()
            .copied()
            .find(|p| p.release == token || p.version == token)
            .ok_or_else(|| anyhow::anyhow!("no Ubuntu product matches release token {token:?}"))?
    };

    // Serial keys are date-stamped (`YYYYMMDD` or `YYYYMMDD.N`), so
    // lexicographic ordering picks the newest build correctly.
    let (serial, ver) = product
        .versions
        .iter()
        .next_back()
        .ok_or_else(|| anyhow::anyhow!("no versions for {}", product.release))?;

    let item = ver
        .items
        .get("disk1.img")
        .ok_or_else(|| anyhow::anyhow!("no disk1.img item for {}/{serial}", product.release))?;
    let sha256 = item
        .sha256
        .clone()
        .ok_or_else(|| anyhow::anyhow!("disk1.img item missing sha256"))?;

    let url = Url::parse(&format!("{BASE_URL}{}", item.path)).context("ubuntu disk1.img url")?;

    Ok(StreamsImage {
        codename: product.release.clone(),
        release_title: product.release_title.clone(),
        serial: serial.clone(),
        url,
        sha256,
    })
}

fn is_lts(title: &str) -> bool {
    title.contains("LTS")
}

/// Catalog every amd64 product as a [`VersionEntry`], newest-first.
/// `supported` comes straight from the stream's `supported` flag (the
/// maintained set, which includes the LTS releases). The `token` is the
/// codename, which [`super::Ubuntu::resolve`] accepts.
pub fn catalog(streams: &Streams) -> Vec<VersionEntry> {
    let mut entries: Vec<VersionEntry> = streams
        .products
        .values()
        .filter(|p| p.arch == "amd64")
        .map(|p| {
            let title = if p.release_title.is_empty() {
                p.version.clone()
            } else {
                p.release_title.clone()
            };
            VersionEntry {
                token: p.release.clone(),
                series: p.release.clone(),
                version: p.version.clone(),
                title,
                eol_date: p
                    .support_eol
                    .as_deref()
                    .and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()),
                supported: p.supported,
                lts: p.release_title.contains("LTS"),
                dev: None,
                channel: false,
            }
        })
        .collect();
    // Newest-first: version keys are dotted `YY.MM`, so lexicographic
    // descending order matches release recency.
    entries.sort_by(|a, b| b.version.cmp(&a.version));
    entries
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn fixture() -> Streams {
        let json = r#"{
          "products": {
            "com.ubuntu.cloud:server:22.04:amd64": {
              "arch": "amd64",
              "release": "jammy",
              "release_title": "22.04 LTS",
              "supported": true,
              "version": "22.04",
              "versions": {
                "20260101": {
                  "items": {
                    "disk1.img": {
                      "path": "server/releases/jammy/release-20260101/ubuntu-22.04-server-cloudimg-amd64.img",
                      "sha256": "aaa",
                      "size": 1
                    }
                  }
                }
              }
            },
            "com.ubuntu.cloud:server:24.04:amd64": {
              "arch": "amd64",
              "release": "noble",
              "release_title": "24.04 LTS",
              "supported": true,
              "version": "24.04",
              "versions": {
                "20260201": { "items": { "disk1.img": { "path": "p1", "sha256": "old", "size": 1 } } },
                "20260301": { "items": { "disk1.img": { "path": "server/releases/noble/release-20260301/ubuntu-24.04-server-cloudimg-amd64.img", "sha256": "bbb", "size": 2 } } }
              }
            },
            "com.ubuntu.cloud:server:24.10:amd64": {
              "arch": "amd64",
              "release": "oracular",
              "release_title": "24.10",
              "supported": true,
              "version": "24.10",
              "versions": {
                "20260101": {
                  "items": {
                    "disk1.img": {
                      "path": "server/releases/oracular/release-20260101/ubuntu-24.10-server-cloudimg-amd64.img",
                      "sha256": "ccc",
                      "size": 1
                    }
                  }
                }
              }
            },
            "com.ubuntu.cloud:server:22.04:arm64": {
              "arch": "arm64",
              "release": "jammy",
              "release_title": "22.04 LTS",
              "supported": true,
              "version": "22.04",
              "versions": {}
            }
          }
        }"#;
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn latest_picks_newest_lts() {
        let s = fixture();
        let img = resolve(&s, "latest").unwrap();
        assert_eq!(img.codename, "noble");
        assert_eq!(img.serial, "20260301");
        assert_eq!(img.sha256, "bbb");
    }

    #[test]
    fn latest_skips_non_lts() {
        let s = fixture();
        let img = resolve(&s, "latest").unwrap();
        assert_ne!(img.codename, "oracular");
    }

    #[test]
    fn resolve_by_codename_picks_latest_serial() {
        let s = fixture();
        let img = resolve(&s, "noble").unwrap();
        assert_eq!(img.codename, "noble");
        assert_eq!(img.serial, "20260301");
    }

    #[test]
    fn resolve_by_version() {
        let s = fixture();
        let img = resolve(&s, "22.04").unwrap();
        assert_eq!(img.codename, "jammy");
    }

    #[test]
    fn resolve_unknown_errors() {
        let s = fixture();
        assert!(resolve(&s, "zzz").is_err());
    }

    #[test]
    fn url_is_built_from_base_plus_path() {
        let s = fixture();
        let img = resolve(&s, "noble").unwrap();
        assert_eq!(
            img.url.as_str(),
            "https://cloud-images.ubuntu.com/server/releases/noble/release-20260301/ubuntu-24.04-server-cloudimg-amd64.img"
        );
    }

    #[test]
    fn catalog_carries_support_eol() {
        let json = r#"{"products": {"p": {
            "arch": "amd64", "release": "noble", "release_title": "24.04 LTS",
            "supported": true, "version": "24.04", "support_eol": "2029-05-31",
            "versions": {}}}}"#;
        let streams: Streams = serde_json::from_str(json).unwrap();
        assert_eq!(
            catalog(&streams)[0].eol_date,
            chrono::NaiveDate::from_ymd_opt(2029, 5, 31)
        );
        // The fixture's products have no support_eol.
        assert!(catalog(&fixture()).iter().all(|e| e.eol_date.is_none()));
    }

    #[test]
    fn catalog_is_newest_first() {
        let product = |release: &str, version: &str| {
            format!(
                r#""{release}": {{"arch": "amd64", "release": "{release}",
                "supported": true, "version": "{version}", "versions": {{}}}}"#
            )
        };
        let json = format!(
            r#"{{"products": {{{}, {}, {}}}}}"#,
            product("jammy", "22.04"),
            product("resolute", "26.04"),
            product("noble", "24.04")
        );
        let streams: Streams = serde_json::from_str(&json).unwrap();
        let tokens: Vec<String> = catalog(&streams).into_iter().map(|e| e.token).collect();
        assert_eq!(tokens, ["resolute", "noble", "jammy"]);
    }

    #[test]
    fn catalog_flags_releases_ubuntu_labels_lts() {
        let lts: Vec<(String, bool)> = catalog(&fixture())
            .into_iter()
            .map(|e| (e.token, e.lts))
            .collect();
        assert!(
            lts.iter().all(|(token, lts)| *lts == (token != "oracular")),
            "{lts:?}"
        );
    }
}
