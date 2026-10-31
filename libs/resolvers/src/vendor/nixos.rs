// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! NixOS cloud-image vendor profile.
//!
//! The NixOS project publishes no image for SmartOS, so this is a
//! community build, and the index names the distro "NixOS (community
//! build)". Uses the builds at `nshalman/nixos-cloud-image` GitHub
//! releases: nixpkgs' UEFI `qemu-efi` image with cloud-init (NoCloud)
//! set up for SmartOS bhyve guests, built and boot-tested by that
//! repository's GitHub Actions. Two channels: `stable`, builds of the
//! newest NixOS release, and `unstable`, builds of `nixos-unstable`.

mod releases;

use anyhow::{Context, Result};
use async_trait::async_trait;
use url::Url;

use super::{
    ImageFacts, ResolvedImage, SourceFormat, VendorProfile, VersionEntry, checksum_document,
};
use crate::verify::{Sha256Pinned, SumsStyle};
use releases::Channel;

pub struct Nixos;

#[async_trait]
impl VendorProfile for Nixos {
    fn name(&self) -> &str {
        "nixos"
    }

    async fn list_versions(&self, _http: &reqwest::Client) -> Result<Vec<VersionEntry>> {
        // Both channels are resolved from the release list; which NixOS
        // release each is on is only known then, so no network read
        // here.
        let entry = |channel: &str, title: &str, supported: bool| VersionEntry {
            token: channel.to_string(),
            series: channel.to_string(),
            version: channel.to_string(),
            title: title.to_string(),
            eol_date: None,
            supported,
            lts: false,
            dev: (channel == "unstable").then(|| channel.to_string()),
            channel: true,
        };
        Ok(vec![
            entry("stable", "NixOS stable", true),
            entry("unstable", "NixOS unstable (development channel)", false),
        ])
    }

    async fn resolve_release(
        &self,
        release: &str,
        http: &reqwest::Client,
    ) -> Result<ResolvedImage> {
        let channel = Channel::parse(release)?;
        let resolved = releases::resolve(http, channel).await?;
        let url: Url = resolved.url.parse().context("nixos image url")?;
        let release_name = channel.release_name(&resolved.series);
        let nixos_version = resolved
            .tag
            .split_once('-')
            .map_or(resolved.tag.as_str(), |(version, _)| version);

        Ok(ResolvedImage {
            url,
            format: SourceFormat::Qcow2,
            os: "linux".to_string(),
            series: release_name.clone(),
            // The full tag, so each build of a channel is distinct.
            version: resolved.tag.clone(),
            description: format!(
                "NixOS {nixos_version} ({}) CloudInit NoCloud compatible image (community \
                 build). Built to run on bhyve virtual machines.",
                release.trim()
            ),
            homepage: Url::parse("https://github.com/nshalman/nixos-cloud-image")
                .context("nixos homepage url")?,
            ssh_key: true,
            verifier: Box::new(Sha256Pinned::from_document(
                resolved.sha256.clone(),
                checksum_document(&resolved.checksum_url, releases::ASSET_NAME, SumsStyle::Gnu)?,
            )),
            expected_sha256: Some(resolved.sha256),
            facts: ImageFacts {
                release: Some(release_name),
                published_at: resolved.published_at,
                ..ImageFacts::default()
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn channels_report_their_labels() -> Result<()> {
        let http = crate::test_client_builder().build()?;
        let entries = Nixos.list_versions(&http).await?;
        let labels: Vec<(&str, bool, bool, Option<&str>)> = entries
            .iter()
            .map(|e| (e.token.as_str(), e.channel, e.supported, e.dev.as_deref()))
            .collect();
        assert_eq!(
            labels,
            [
                ("stable", true, true, None),
                ("unstable", true, false, Some("unstable")),
            ]
        );
        Ok(())
    }
}
