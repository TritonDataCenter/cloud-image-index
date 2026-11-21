// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! NixOS cloud-image release discovery.
//!
//! Images are published as GitHub release assets at
//! `nshalman/nixos-cloud-image`, one release per build, each holding
//! `nixos-smartos-x86_64.qcow2` and its `.sha256` sidecar. Builds of
//! the newest NixOS release are full releases; builds of
//! `nixos-unstable` are pre-releases. Tags look like
//! `26.05.20261026.718f670-9h4726gw`: the NixOS version, then the
//! first 8 characters of the image derivation's hash. The repository
//! keeps a few builds of each, so a channel resolves to the newest
//! published release of its kind in the release list.
//!
//! Trust roots in TLS to `api.github.com` / `github.com`. The sidecar
//! is GNU style, `<hex>  nixos-smartos-x86_64.qcow2`.

use anyhow::Result;

use super::super::github_release::{self, Release};

const REPO: &str = "nshalman/nixos-cloud-image";
pub const ASSET_NAME: &str = "nixos-smartos-x86_64.qcow2";

/// The two channels the repository publishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// Builds of the newest NixOS release (e.g. `nixos-26.05`).
    Stable,
    /// Builds of `nixos-unstable`, published as pre-releases.
    Unstable,
}

impl Channel {
    pub fn parse(token: &str) -> Result<Channel> {
        match token.trim() {
            "stable" => Ok(Channel::Stable),
            "unstable" => Ok(Channel::Unstable),
            _ => anyhow::bail!("nixos: expected 'stable' or 'unstable', got {token:?}"),
        }
    }

    /// The index release a build of this channel belongs to: the NixOS
    /// release (`26.05`) for stable, and `unstable` for unstable, whose
    /// series names a release that does not exist yet and, while a new
    /// release is being adopted, may equal stable's.
    pub fn release_name(&self, series: &str) -> String {
        match self {
            Channel::Stable => series.to_string(),
            Channel::Unstable => "unstable".to_string(),
        }
    }
}

#[derive(Debug)]
pub struct Resolved {
    /// The NixOS release the build is of (e.g. `26.05`, or `26.11` for
    /// an unstable build before 26.11 is released).
    pub series: String,
    /// Full upstream tag (e.g. `26.05.20261026.718f670-9h4726gw`).
    pub tag: String,
    pub url: String,
    pub sha256: String,
    /// The checksum sidecar asset the hash was read from.
    pub checksum_url: String,
    /// When GitHub says the release was published.
    pub published_at: Option<chrono::DateTime<chrono::Utc>>,
}

pub async fn resolve(http: &reqwest::Client, channel: Channel) -> Result<Resolved> {
    let url = format!("https://api.github.com/repos/{REPO}/releases?per_page=100");
    let releases: Vec<Release> = github_release::get(http, &url).await?;
    let release = newest(channel, releases)?;
    let series = series_of_tag(&release.tag_name)
        .ok_or_else(|| anyhow::anyhow!("nixos: unexpected tag {:?}", release.tag_name))?;
    let asset = github_release::pinned_asset(http, &release, ASSET_NAME).await?;

    Ok(Resolved {
        series,
        tag: release.tag_name,
        url: asset.url,
        sha256: asset.sha256,
        checksum_url: asset.checksum_url,
        published_at: release.published_at,
    })
}

/// The newest published release of `channel`'s kind whose tag has the
/// repository's shape. Drafts have no publish date and are skipped; the
/// list's order is not relied on.
fn newest(channel: Channel, releases: Vec<Release>) -> Result<Release> {
    releases
        .into_iter()
        .filter(|r| r.prerelease == (channel == Channel::Unstable))
        .filter(|r| series_of_tag(&r.tag_name).is_some())
        .filter(|r| r.published_at.is_some())
        .max_by_key(|r| r.published_at)
        .ok_or_else(|| anyhow::anyhow!("nixos: no published {channel:?} release"))
}

/// The NixOS release (`YY.MM`) at the start of a tag like
/// `26.05.20261026.718f670-9h4726gw`.
fn series_of_tag(tag: &str) -> Option<String> {
    let (version, hash) = tag.split_once('-')?;
    let mut parts = version.split('.');
    let year = parts.next()?;
    let month = parts.next()?;
    let two_digits = |s: &str| s.len() == 2 && s.bytes().all(|b| b.is_ascii_digit());
    (two_digits(year) && two_digits(month) && parts.next().is_some() && !hash.is_empty())
        .then(|| format!("{year}.{month}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn release(tag: &str, prerelease: bool, published_at: Option<&str>) -> Release {
        serde_json::from_value(serde_json::json!({
            "tag_name": tag,
            "prerelease": prerelease,
            "published_at": published_at,
            "assets": [],
        }))
        .unwrap()
    }

    #[test]
    fn channel_parses_the_two_tokens() {
        assert_eq!(Channel::parse("stable").unwrap(), Channel::Stable);
        assert_eq!(Channel::parse("unstable").unwrap(), Channel::Unstable);
        assert!(Channel::parse("latest").is_err());
        assert!(Channel::parse("26.05").is_err());
        assert!(Channel::parse("").is_err());
    }

    #[test]
    fn stable_is_named_by_series_and_unstable_by_channel() {
        assert_eq!(Channel::Stable.release_name("26.05"), "26.05");
        assert_eq!(Channel::Unstable.release_name("26.11"), "unstable");
    }

    #[test]
    fn series_of_tag_takes_the_nixos_release() {
        assert_eq!(
            series_of_tag("26.05.20261026.718f670-9h4726gw").unwrap(),
            "26.05"
        );
        assert_eq!(
            series_of_tag("26.11.20261023.3ec6363-piivn8mp").unwrap(),
            "26.11"
        );
        assert!(series_of_tag("26.05").is_none());
        assert!(series_of_tag("v26.05.20261026.718f670-9h4726gw").is_none());
        assert!(series_of_tag("2605.20261026.718f670-9h4726gw").is_none());
        assert!(series_of_tag("26.5.20261026.718f670-9h4726gw").is_none());
        assert!(series_of_tag("latest").is_none());
    }

    fn sample() -> Vec<Release> {
        vec![
            release(
                "26.11.20261102.aaaaaaa-bbbbbbbb",
                true,
                Some("2026-11-02T05:40:00Z"),
            ),
            release(
                "26.05.20261026.718f670-9h4726gw",
                false,
                Some("2026-10-27T13:00:00Z"),
            ),
            // Older, but listed after a newer one: order is not trusted.
            release(
                "26.11.20261023.3ec6363-piivn8mp",
                true,
                Some("2026-10-27T13:01:00Z"),
            ),
            release(
                "26.05.20261101.ccccccc-dddddddd",
                false,
                Some("2026-11-01T06:40:00Z"),
            ),
            // A draft (no publish date) and a tag of another shape are
            // never picked, however new.
            release("26.05.20261201.eeeeeee-ffffffff", false, None),
            release("v-something-else", false, Some("2026-12-01T00:00:00Z")),
            release("not-ours", true, Some("2026-12-01T00:00:00Z")),
        ]
    }

    #[test]
    fn stable_is_the_newest_published_full_release() {
        assert_eq!(
            newest(Channel::Stable, sample()).unwrap().tag_name,
            "26.05.20261101.ccccccc-dddddddd"
        );
    }

    #[test]
    fn unstable_is_the_newest_published_prerelease() {
        assert_eq!(
            newest(Channel::Unstable, sample()).unwrap().tag_name,
            "26.11.20261102.aaaaaaa-bbbbbbbb"
        );
    }

    #[test]
    fn a_channel_with_no_release_is_an_error() {
        let only_stable = vec![release(
            "26.05.20261026.718f670-9h4726gw",
            false,
            Some("2026-10-27T13:00:00Z"),
        )];
        assert!(newest(Channel::Unstable, only_stable).is_err());
        assert!(newest(Channel::Stable, Vec::new()).is_err());
    }
}
