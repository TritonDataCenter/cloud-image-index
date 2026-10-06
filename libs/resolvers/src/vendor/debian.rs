// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Debian cloud-image vendor profile.
//!
//! Debian publishes generic cloud images in dated build directories,
//! `https://cloud.debian.org/images/cloud/<codename>/<YYYYMMDD-NNNN>/`
//! (`latest/` is a rolling copy of the newest). We resolve to the
//! newest dated build so the image is an exact build. We pick
//! the `genericcloud` qcow2 — its cloud-init auto-detects the NoCloud
//! datasource SmartOS provides on bhyve. The sibling `SHA512SUMS`
//! file is published in SHA-512 (not SHA-256), so we use the
//! `Sha512SumsTls` verifier.
//!
//! Release resolution consults Debian's apt `Release` file at
//! `https://deb.debian.org/debian/dists/<suite>/Release` — the same
//! file apt itself uses to know what `stable` means today. This lets
//! the user pass any of:
//!
//! - `latest` — alias for `stable`
//! - symbolic suite names — `stable`, `oldstable`, `oldoldstable`
//! - codenames — `trixie`, `bookworm`, `bullseye`, ...
//!
//! `testing` and `unstable` (and their codenames) do not resolve: their
//! Release files carry no `Version` field.
//!
//! Since the build downloads a multi-hundred-megabyte image over the
//! same network, requiring an additional small Release-file fetch
//! adds no real fragility, and we use its `Codename` and `Version`
//! fields directly. No hardcoded codename table.

mod release_file;

use anyhow::{Context, Result};
use async_trait::async_trait;
use url::Url;

use super::{ImageFacts, ResolvedImage, SourceFormat, VendorProfile, VersionEntry};
use crate::verify::Sha512SumsTls;

pub struct Debian;

const CLOUD_BASE: &str = "https://cloud.debian.org/images/cloud/";

/// The newest dated build directory (`YYYYMMDD-NNNN`) in a codename's
/// directory listing.
fn newest_build_dir(listing: &str) -> Option<String> {
    let re = regex::Regex::new(r#"href="(\d{8}-\d+)/""#).ok()?;
    re.captures_iter(listing)
        .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
        .max_by_key(|b| {
            let (date, n) = b.split_once('-').unwrap_or((b, "0"));
            (date.to_string(), n.parse::<u64>().unwrap_or(0))
        })
}

/// The point release a dated build (`YYYYMMDD-NNNN`) contains: the
/// suite's current point release `version` if the build is from after
/// the day it was released, otherwise unknown. A build from that same
/// day may predate the release, so it is unknown too.
fn point_release_of_build(
    version: &str,
    released: Option<chrono::NaiveDate>,
    build: &str,
) -> Option<String> {
    let built = chrono::NaiveDate::parse_from_str(build.get(..8)?, "%Y%m%d").ok()?;
    (built > released?).then(|| version.to_string())
}

/// Image URL, SHA512SUMS URL and image filename for one dated build.
fn build_urls(codename: &str, major: u32, build: &str) -> Result<(Url, Url, String)> {
    let dir = format!("{CLOUD_BASE}{codename}/{build}/");
    let filename = format!("debian-{major}-genericcloud-amd64-{build}.qcow2");
    let url: Url = format!("{dir}{filename}")
        .parse()
        .context("debian image url")?;
    let sums_url: Url = format!("{dir}SHA512SUMS")
        .parse()
        .context("debian SHA512SUMS url")?;
    Ok((url, sums_url, filename))
}

/// Apt suites the catalog probes, with whether they're a maintained
/// (supported) release. Each is resolved through the same
/// `dists/<suite>/Release` file the resolver reads, so the codename +
/// version are upstream-current, not hardcoded. `testing` and
/// `unstable` are not listed: their Release files have no `Version`.
const SUITES: &[(&str, bool)] = &[("stable", true), ("oldstable", true)];

/// The version list from each suite's Release file, in [`SUITES`]
/// order. Any suite that could not be read fails the whole list:
/// skipping it would quietly move `latest` to the next suite.
fn catalog(
    fetched: Vec<(&str, bool, Result<release_file::ReleaseInfo>)>,
) -> Result<Vec<VersionEntry>> {
    fetched
        .into_iter()
        .map(|(suite, supported, info)| {
            let info = info.with_context(|| format!("debian: apt suite {suite}"))?;
            Ok(VersionEntry {
                token: info.codename.clone(),
                series: info.codename.clone(),
                version: info.version.clone(),
                title: format!("Debian {} ({}) — {suite}", info.version, info.codename),
                eol_date: None,
                supported,
                lts: false,
                dev: None,
                channel: false,
            })
        })
        .collect()
}

/// Translate the user-facing release token to the suite path component
/// used in the apt Release URL `dists/<suite>/Release`. `latest` is
/// the only alias we own; everything else passes through unchanged
/// and either resolves at upstream or 404s with a clear error.
fn token_to_suite(release: &str) -> &str {
    match release.trim() {
        "latest" => "stable",
        other => other,
    }
}

#[async_trait]
impl VendorProfile for Debian {
    fn name(&self) -> &str {
        "debian"
    }

    async fn list_versions(&self, http: &reqwest::Client) -> Result<Vec<VersionEntry>> {
        // Resolve each symbolic suite's current codename + version via
        // the same apt Release file the resolver uses.
        let mut fetched = Vec::new();
        for (suite, supported) in SUITES {
            fetched.push((*suite, *supported, release_file::fetch(http, suite).await));
        }
        catalog(fetched)
    }

    async fn resolve_release(
        &self,
        release: &str,
        http: &reqwest::Client,
    ) -> Result<ResolvedImage> {
        let suite = token_to_suite(release);
        let info = release_file::fetch(http, suite)
            .await
            .with_context(|| format!("resolve debian {release:?}"))?;

        let codename = info.codename;
        let version = info.version;
        let release_date = info.date;
        let major = release_file::major_of(&version).ok_or_else(|| {
            anyhow::anyhow!(
                "could not parse major version from upstream {version:?} for {codename}"
            )
        })?;

        let codename_dir = format!("{CLOUD_BASE}{codename}/");
        let listing = http
            .get(&codename_dir)
            .send()
            .await
            .with_context(|| format!("GET {codename_dir}"))?
            .error_for_status()
            .with_context(|| format!("status from {codename_dir}"))?
            .text()
            .await
            .with_context(|| format!("read body of {codename_dir}"))?;
        let build = newest_build_dir(&listing)
            .ok_or_else(|| anyhow::anyhow!("no dated build directory under {codename_dir}"))?;
        let (url, sums_url, filename) = build_urls(&codename, major, &build)?;
        let point_release = point_release_of_build(&version, release_date, &build);

        Ok(ResolvedImage {
            url,
            format: SourceFormat::Qcow2,
            os: "linux".to_string(),
            series: codename.clone(),
            // The dated build id (e.g. "20261001-2618") identifies the
            // exact image. This used to be the point release ("13.4"),
            // which kept the manifest version stable across rebuilds of
            // one point release; the index needs exact builds instead.
            // The point release stays in the description.
            version: build,
            description: format!(
                "Debian {version} ({codename}) CloudInit NoCloud compatible image. \
                 Built to run on bhyve virtual machines."
            ),
            homepage: Url::parse("https://www.debian.org/").context("debian homepage url")?,
            ssh_key: true,
            verifier: Box::new(Sha512SumsTls::new(sums_url, filename)),
            // Debian's hash channel is SHA-512, not SHA-256, so no
            // sha256 is known before download.
            expected_sha256: None,
            facts: ImageFacts {
                point_release,
                ..ImageFacts::default()
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_aliases_to_stable() {
        assert_eq!(token_to_suite("latest"), "stable");
    }

    #[test]
    fn codename_passes_through() {
        assert_eq!(token_to_suite("trixie"), "trixie");
        assert_eq!(token_to_suite("bookworm"), "bookworm");
    }

    #[test]
    fn symbolic_suites_pass_through() {
        assert_eq!(token_to_suite("stable"), "stable");
        assert_eq!(token_to_suite("oldstable"), "oldstable");
        assert_eq!(token_to_suite("oldoldstable"), "oldoldstable");
        assert_eq!(token_to_suite("testing"), "testing");
    }

    #[test]
    fn whitespace_is_trimmed() {
        assert_eq!(token_to_suite("  trixie  "), "trixie");
    }

    #[test]
    fn newest_build_dir_picks_the_latest_dated_directory() {
        let listing = r#"
<a href="20260831-2587/">x</a>
<a href="20261001-2618/">x</a>
<a href="20260914-2601/">x</a>
<a href="latest/">x</a>
<a href="daily/">x</a>
"#;
        assert_eq!(newest_build_dir(listing).as_deref(), Some("20261001-2618"));
        assert_eq!(newest_build_dir("<a href=\"latest/\">x</a>"), None);
    }

    #[test]
    fn build_urls_name_the_exact_dated_image() {
        let (url, sums_url, filename) =
            build_urls("trixie", 13, "20261001-2618").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(filename, "debian-13-genericcloud-amd64-20261001-2618.qcow2");
        assert_eq!(
            url.as_str(),
            "https://cloud.debian.org/images/cloud/trixie/20261001-2618/debian-13-genericcloud-amd64-20261001-2618.qcow2"
        );
        assert_eq!(
            sums_url.as_str(),
            "https://cloud.debian.org/images/cloud/trixie/20261001-2618/SHA512SUMS"
        );
    }

    fn info(codename: &str, version: &str) -> Result<release_file::ReleaseInfo> {
        Ok(release_file::ReleaseInfo {
            codename: codename.to_string(),
            version: version.to_string(),
            date: None,
        })
    }

    #[test]
    fn catalog_lists_every_suite_in_order() {
        let entries = catalog(vec![
            ("stable", true, info("trixie", "13.4")),
            ("oldstable", true, info("bookworm", "12.12")),
        ])
        .unwrap_or_else(|e| panic!("{e:#}"));
        let tokens: Vec<&str> = entries.iter().map(|e| e.token.as_str()).collect();
        assert_eq!(tokens, ["trixie", "bookworm"]);
    }

    #[test]
    fn one_failed_suite_fails_the_catalog() {
        // Skipping it would let `latest` fall to oldstable unnoticed.
        let e = catalog(vec![
            ("stable", true, Err(anyhow::anyhow!("503"))),
            ("oldstable", true, info("bookworm", "12.12")),
        ])
        .err()
        .map(|e| format!("{e:#}"))
        .unwrap_or_default();
        assert!(e.contains("stable") && e.contains("503"), "{e}");
    }

    #[test]
    fn a_build_names_the_point_release_only_if_built_after_it() {
        // The apt Release file describes today's point release; a dated
        // build from before it does not contain it.
        let released = chrono::NaiveDate::from_ymd_opt(2026, 9, 12);
        assert_eq!(
            point_release_of_build("13.7", released, "20261001-2618"),
            Some("13.7".to_string())
        );
        assert_eq!(
            point_release_of_build("13.7", released, "20260901-2590"),
            None
        );
        assert_eq!(
            point_release_of_build("13.7", released, "20260912-2600"),
            None,
            "same day: the build may predate the release"
        );
        assert_eq!(point_release_of_build("13.7", None, "20261001-2618"), None);
        assert_eq!(point_release_of_build("13.7", released, "garbage"), None);
    }

    #[test]
    fn suites_are_newest_first() {
        let suites: Vec<&str> = SUITES.iter().map(|(s, _)| *s).collect();
        assert_eq!(suites.first(), Some(&"stable"));
        assert!(
            suites.iter().position(|s| *s == "stable")
                < suites.iter().position(|s| *s == "oldstable")
        );
    }
}
