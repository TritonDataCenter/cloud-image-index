// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Alpine's release metadata feed at `https://alpinelinux.org/releases.json`.
//!
//! The schema is roughly:
//!
//! ```json
//! {
//!   "latest_stable": "v3.23",
//!   "release_branches": [
//!     {
//!       "rel_branch": "v3.23",
//!       "eol_date": "2027-11-01",
//!       "releases": [
//!         { "version": "3.23.4", "date": "..." },
//!         { "version": "3.23.3", "date": "..." }
//!       ]
//!     }
//!   ]
//! }
//! ```
//!
//! Resolution rules for the user-facing release token:
//! - `latest` → `latest_stable` branch, newest release in it.
//! - branch (`3.23` or `v3.23`) → that branch, newest release.
//! - full version (`3.23.4`) → find the branch that contains it.

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::vendor::VersionEntry;

const RELEASES_URL: &str = "https://alpinelinux.org/releases.json";

#[derive(Deserialize)]
pub struct ReleasesJson {
    pub latest_stable: String,
    pub release_branches: Vec<Branch>,
}

#[derive(Deserialize)]
pub struct Branch {
    pub rel_branch: String,
    /// End of support for the branch (`YYYY-MM-DD`), when published.
    #[serde(default)]
    pub eol_date: Option<String>,
    #[serde(default)]
    pub releases: Vec<Release>,
}

#[derive(Deserialize)]
pub struct Release {
    pub version: String,
}

pub struct ResolvedRelease {
    /// Branch name including the `v` prefix, e.g. `"v3.23"`. Used as
    /// the URL path component for `dl-cdn.alpinelinux.org/alpine/<branch>/`.
    pub branch: String,
    /// Full point-release version, e.g. `"3.23.4"`. Used in the
    /// downloaded filename and as the manifest `version` field.
    pub version: String,
}

pub async fn fetch(http: &reqwest::Client) -> Result<ReleasesJson> {
    eprintln!("Fetching Alpine releases.json ...");
    http.get(RELEASES_URL)
        .send()
        .await
        .with_context(|| format!("GET {RELEASES_URL}"))?
        .error_for_status()
        .with_context(|| format!("status from {RELEASES_URL}"))?
        .json::<ReleasesJson>()
        .await
        .with_context(|| format!("parse {RELEASES_URL}"))
}

pub fn resolve(rj: &ReleasesJson, token: &str) -> Result<ResolvedRelease> {
    let token = token.trim();

    if token == "latest" {
        let branch_id = &rj.latest_stable;
        let branch = rj
            .release_branches
            .iter()
            .find(|b| &b.rel_branch == branch_id)
            .ok_or_else(|| {
                anyhow::anyhow!("latest_stable {branch_id:?} not found in release_branches")
            })?;
        let release = branch
            .releases
            .first()
            .ok_or_else(|| anyhow::anyhow!("no releases in branch {}", branch.rel_branch))?;
        return Ok(ResolvedRelease {
            branch: branch.rel_branch.clone(),
            version: release.version.clone(),
        });
    }

    // Branch token: accept "3.23" or "v3.23".
    let branch_id = if token.starts_with('v') {
        token.to_string()
    } else {
        format!("v{token}")
    };
    if let Some(branch) = rj
        .release_branches
        .iter()
        .find(|b| b.rel_branch == branch_id)
    {
        let release = branch
            .releases
            .first()
            .ok_or_else(|| anyhow::anyhow!("no releases in branch {}", branch.rel_branch))?;
        return Ok(ResolvedRelease {
            branch: branch.rel_branch.clone(),
            version: release.version.clone(),
        });
    }

    // Full version token: search all branches.
    if token.matches('.').count() == 2 {
        for branch in &rj.release_branches {
            if branch.releases.iter().any(|r| r.version == token) {
                return Ok(ResolvedRelease {
                    branch: branch.rel_branch.clone(),
                    version: token.to_string(),
                });
            }
        }
        anyhow::bail!("alpine: version {token:?} not found in any release branch");
    }

    anyhow::bail!(
        "alpine: unknown release token {token:?}; try 'latest', a branch like '3.23', \
         or a full version like '3.23.4'"
    );
}

/// A NoCloud cloud image found in a branch's `releases/cloud/` listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudImage {
    pub filename: String,
    /// Point release the image was built from (e.g. `3.23.4`). Alpine
    /// does not build cloud images for every point release.
    pub version: String,
}

/// Pick the x86_64 cloud-init qcow2 from a branch's
/// `releases/cloud/` directory listing: the newest point release (or
/// exactly `want_version`), then the highest `-r<N>` rebuild. Two
/// namings are recognised:
///
/// - `alpine-<ver>-x86_64-cloudinit-r<N>.qcow2` (3.24 onwards; its
///   metadata lists `nocloud` among the supported clouds)
/// - `nocloud_alpine-<ver>-x86_64-uefi-cloudinit-r<N>.qcow2` (earlier)
///
/// `-metal`, `-tiny`, `bios` and cloud-specific (`generic_`, `aws_`,
/// ...) images are not picked.
pub fn pick_cloud_image(listing: &str, want_version: Option<&str>) -> Option<CloudImage> {
    let re = regex::Regex::new(
        r#"href="((?:nocloud_)?alpine-(\d+)\.(\d+)\.(\d+)-x86_64-(uefi-)?cloudinit-r(\d+)\.qcow2)""#,
    )
    .ok()?;
    re.captures_iter(listing)
        .filter_map(|c| {
            let filename = c.get(1)?.as_str();
            let nocloud_prefix = filename.starts_with("nocloud_");
            // The old naming is `nocloud_...-uefi-cloudinit`; the new
            // one has neither the prefix nor a firmware component.
            if nocloud_prefix != c.get(5).is_some() {
                return None;
            }
            let num = |i| c.get(i)?.as_str().parse::<u32>().ok();
            let key = (num(2)?, num(3)?, num(4)?, num(6)?);
            let version = format!("{}.{}.{}", key.0, key.1, key.2);
            Some((key, version, filename.to_string()))
        })
        .filter(|(_, version, _)| want_version.is_none_or(|w| w == version))
        .max_by_key(|(key, _, _)| *key)
        .map(|(_, version, filename)| CloudImage { filename, version })
}

/// Catalog each release branch as a [`VersionEntry`], newest first
/// (sorted here rather than trusting the feed's order). The `token` is the `v`-less
/// branch (e.g. `3.23`), which [`resolve`] accepts; the `version` is
/// the newest point release in that branch. A branch is supported until
/// its `eol_date`; a branch without one counts as supported.
pub fn catalog(rj: &ReleasesJson, today: chrono::NaiveDate) -> Vec<VersionEntry> {
    let mut entries: Vec<VersionEntry> = rj
        .release_branches
        .iter()
        .filter_map(|b| {
            let version = b.releases.first()?.version.clone();
            let series = b
                .rel_branch
                .strip_prefix('v')
                .unwrap_or(&b.rel_branch)
                .to_string();
            let eol_date = b
                .eol_date
                .as_deref()
                .and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok());
            Some(VersionEntry {
                token: series.clone(),
                series: series.clone(),
                version,
                title: format!("Alpine Linux v{series}"),
                eol_date,
                supported: eol_date.is_none_or(|eol| eol > today),
                lts: false,
                dev: None,
                channel: false,
            })
        })
        .collect();
    let key =
        |token: &str| -> Vec<u32> { token.split('.').map(|p| p.parse().unwrap_or(0)).collect() };
    entries.sort_by_key(|e| std::cmp::Reverse(key(&e.token)));
    entries
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn fixture() -> ReleasesJson {
        let json = r#"{
          "latest_stable": "v3.23",
          "release_branches": [
            {
              "rel_branch": "v3.23",
              "releases": [
                {"version": "3.23.4"},
                {"version": "3.23.3"}
              ]
            },
            {
              "rel_branch": "v3.22",
              "releases": [
                {"version": "3.22.6"},
                {"version": "3.22.5"}
              ]
            }
          ]
        }"#;
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn resolve_latest_picks_newest_release_in_latest_stable() {
        let r = resolve(&fixture(), "latest").unwrap();
        assert_eq!(r.branch, "v3.23");
        assert_eq!(r.version, "3.23.4");
    }

    #[test]
    fn resolve_branch_without_v() {
        let r = resolve(&fixture(), "3.22").unwrap();
        assert_eq!(r.branch, "v3.22");
        assert_eq!(r.version, "3.22.6");
    }

    #[test]
    fn resolve_branch_with_v() {
        let r = resolve(&fixture(), "v3.23").unwrap();
        assert_eq!(r.branch, "v3.23");
        assert_eq!(r.version, "3.23.4");
    }

    #[test]
    fn resolve_full_version_finds_branch() {
        let r = resolve(&fixture(), "3.22.5").unwrap();
        assert_eq!(r.branch, "v3.22");
        assert_eq!(r.version, "3.22.5");
    }

    #[test]
    fn resolve_unknown_branch_errors() {
        assert!(resolve(&fixture(), "3.99").is_err());
    }

    #[test]
    fn resolve_unknown_full_version_errors() {
        assert!(resolve(&fixture(), "3.23.99").is_err());
    }

    fn fixture_with_eol() -> ReleasesJson {
        let json = r#"{
          "latest_stable": "v3.24",
          "release_branches": [
            {"rel_branch": "edge", "releases": []},
            {"rel_branch": "v3.24", "eol_date": "2028-06-01",
             "releases": [{"version": "3.24.2"}]},
            {"rel_branch": "v3.20", "eol_date": "2026-04-01",
             "releases": [{"version": "3.20.10"}]}
          ]
        }"#;
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn catalog_marks_branches_past_eol_unsupported() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 10, 5).unwrap();
        let entries = catalog(&fixture_with_eol(), today);
        let supported: Vec<(&str, bool)> = entries
            .iter()
            .map(|e| (e.token.as_str(), e.supported))
            .collect();
        assert_eq!(supported, vec![("3.24", true), ("3.20", false)]);
    }

    #[test]
    fn catalog_publishes_the_branch_eol_date() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 10, 5).unwrap();
        let eol: Vec<_> = catalog(&fixture_with_eol(), today)
            .into_iter()
            .map(|e| e.eol_date)
            .collect();
        assert_eq!(
            eol,
            vec![
                chrono::NaiveDate::from_ymd_opt(2028, 6, 1),
                chrono::NaiveDate::from_ymd_opt(2026, 4, 1),
            ]
        );
    }

    #[test]
    fn catalog_without_eol_date_counts_as_supported() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 10, 5).unwrap();
        assert!(catalog(&fixture(), today).iter().all(|e| e.supported));
    }

    const LISTING_NEW: &str = r#"
<a href="alpine-3.24.2-aarch64-cloudinit-r0.qcow2">x</a>
<a href="alpine-3.24.2-x86_64-cloudinit-metal-r0.qcow2">x</a>
<a href="alpine-3.24.2-x86_64-cloudinit-r0.qcow2">x</a>
<a href="alpine-3.24.2-x86_64-cloudinit-r0.qcow2.sha512">x</a>
<a href="alpine-3.24.2-x86_64-tiny-r0.qcow2">x</a>
<a href="generic_alpine-3.24.1-x86_64-uefi-cloudinit-r0.qcow2">x</a>
"#;

    const LISTING_OLD: &str = r#"
<a href="nocloud_alpine-3.23.0-x86_64-uefi-cloudinit-r0.qcow2">x</a>
<a href="nocloud_alpine-3.23.4-x86_64-bios-cloudinit-r0.qcow2">x</a>
<a href="nocloud_alpine-3.23.4-x86_64-uefi-cloudinit-metal-r0.qcow2">x</a>
<a href="nocloud_alpine-3.23.4-x86_64-uefi-cloudinit-r0.qcow2">x</a>
<a href="nocloud_alpine-3.23.10-x86_64-uefi-tiny-r0.qcow2">x</a>
<a href="nocloud_alpine-3.23.3-x86_64-uefi-cloudinit-r1.qcow2">x</a>
"#;

    #[test]
    fn pick_image_reads_the_current_naming() {
        let img = pick_cloud_image(LISTING_NEW, None).unwrap();
        assert_eq!(img.filename, "alpine-3.24.2-x86_64-cloudinit-r0.qcow2");
        assert_eq!(img.version, "3.24.2");
    }

    #[test]
    fn pick_image_reads_the_older_nocloud_naming_and_compares_numerically() {
        let img = pick_cloud_image(LISTING_OLD, None).unwrap();
        assert_eq!(
            img.filename,
            "nocloud_alpine-3.23.4-x86_64-uefi-cloudinit-r0.qcow2"
        );
        assert_eq!(img.version, "3.23.4");
    }

    #[test]
    fn pick_image_honours_a_requested_point_release() {
        let img = pick_cloud_image(LISTING_OLD, Some("3.23.0")).unwrap();
        assert_eq!(img.version, "3.23.0");
        assert!(pick_cloud_image(LISTING_OLD, Some("3.23.6")).is_none());
    }

    #[test]
    fn pick_image_finds_nothing_in_an_empty_listing() {
        assert!(pick_cloud_image("<html></html>", None).is_none());
    }

    #[test]
    fn catalog_is_newest_first_whatever_the_feed_order() {
        let json = r#"{
          "latest_stable": "v3.24",
          "release_branches": [
            {"rel_branch": "v3.9", "releases": [{"version": "3.9.6"}]},
            {"rel_branch": "v3.24", "releases": [{"version": "3.24.2"}]},
            {"rel_branch": "v3.10", "releases": [{"version": "3.10.9"}]}
          ]
        }"#;
        let rj: ReleasesJson = serde_json::from_str(json).unwrap();
        let today = chrono::NaiveDate::from_ymd_opt(2026, 10, 5).unwrap();
        let tokens: Vec<String> = catalog(&rj, today).into_iter().map(|e| e.token).collect();
        assert_eq!(tokens, ["3.24", "3.10", "3.9"]);
    }
}
