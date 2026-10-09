// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! OmniOS cloud-image vendor profile.
//!
//! OmniOS publishes a single `omnios-<id>.cloud.vmdk` per release
//! channel at `https://downloads.omnios.org/media/<channel>/`,
//! with channels `stable`, `lts`, and `bloody`. Each ships a
//! sibling bare-hash `<file>.sha256` sidecar.
//!
//! The release-resolution path pre-fetches the sha256, so callers know
//! it without downloading the image. Converting the VMDK is left to
//! the client.

mod releases;

use anyhow::{Context, Result};
use async_trait::async_trait;
use url::Url;

use super::{
    ImageFacts, ResolvedImage, SourceFormat, VendorProfile, VersionEntry, checksum_document,
};
use crate::verify::{Sha256Pinned, SumsStyle};

pub struct Omnios;

#[async_trait]
impl VendorProfile for Omnios {
    fn name(&self) -> &str {
        "omnios"
    }

    async fn list_versions(&self, _http: &reqwest::Client) -> Result<Vec<VersionEntry>> {
        // The resolver accepts the three published channel tokens
        // (`stable`/`lts`/`bloody`); `bloody` is a bleeding-edge weekly
        // snapshot, not a maintained release. No network read needed.
        let entry = |channel: &str, title: &str, supported: bool| VersionEntry {
            token: channel.to_string(),
            series: channel.to_string(),
            version: channel.to_string(),
            title: title.to_string(),
            eol_date: None,
            supported,
            lts: channel == "lts",
            dev: (channel == "bloody").then(|| channel.to_string()),
            channel: true,
        };
        Ok(vec![
            entry("stable", "OmniOS stable", true),
            entry("lts", "OmniOS LTS", true),
            entry("bloody", "OmniOS bloody (dev snapshot)", false),
        ])
    }

    async fn resolve_release(
        &self,
        release: &str,
        http: &reqwest::Client,
    ) -> Result<ResolvedImage> {
        let resolved = releases::resolve(http, release).await?;
        let url: Url = resolved.url.parse().context("omnios image url")?;

        Ok(ResolvedImage {
            url,
            format: SourceFormat::Vmdk,
            os: "illumos".to_string(),
            series: resolved.channel.clone(),
            version: resolved.build.clone(),
            description: format!(
                "OmniOS {} {} CloudInit NoCloud compatible image. \
                 Built to run on bhyve virtual machines.",
                resolved.channel, resolved.build
            ),
            homepage: Url::parse("https://omnios.org/").context("omnios homepage url")?,
            ssh_key: true,
            verifier: Box::new(Sha256Pinned::from_document(
                resolved.sha256.clone(),
                checksum_document(&resolved.checksum_url, "", SumsStyle::Bare)?,
            )),
            expected_sha256: Some(resolved.sha256),
            facts: ImageFacts {
                release: Some(resolved.build.clone()),
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
        let entries = Omnios.list_versions(&http).await?;
        let labels: Vec<(&str, bool, bool, Option<&str>)> = entries
            .iter()
            .map(|e| (e.token.as_str(), e.channel, e.lts, e.dev.as_deref()))
            .collect();
        assert_eq!(
            labels,
            [
                ("stable", true, false, None),
                ("lts", true, true, None),
                ("bloody", true, false, Some("bloody")),
            ]
        );
        Ok(())
    }
}
