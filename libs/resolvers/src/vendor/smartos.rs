// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! SmartOS cloud-image vendor profile.
//!
//! SmartOS publishes per-release artifacts at
//! `https://us-central.manta.mnx.io/Joyent_Dev/public/SmartOS/<release>/`,
//! with a sibling `latest` text file pointing at the current release
//! id. We point at the gzipped raw USB image
//! (`smartos-<rel>-USB.img.gz`) — the same bytes you'd `dd` to a USB
//! stick, no VMware/VMDK detour.
//!
//! SmartOS is **not** cloud-init NoCloud — it provisions guests via
//! the SmartOS metadata service (mdata-get / mdata-put). Including
//! it here is "ouroboros mode": the same machinery that fetches
//! Linux/BSD nocloud images can also turn the upstream SmartOS USB
//! image into a Triton-importable manifest. The `os` field reports
//! `illumos` (matching OmniOS) so consumers don't mistake this for
//! a Triton-native zone image.

mod releases;

use anyhow::{Context, Result};
use async_trait::async_trait;
use url::Url;

use super::{
    ImageFacts, ResolvedImage, SourceFormat, VendorProfile, VersionEntry, checksum_document,
};
use crate::verify::{Sha256Pinned, SumsStyle};

pub struct Smartos;

#[async_trait]
impl VendorProfile for Smartos {
    fn name(&self) -> &str {
        "smartos"
    }

    async fn list_versions(&self, _http: &reqwest::Client) -> Result<Vec<VersionEntry>> {
        // SmartOS is a rolling release; the resolver accepts `latest`
        // (follows the published `latest` pointer) or an explicit
        // `<YYYYMMDD>T<HHMMSS>Z` timestamp. The catalog surfaces the
        // `latest` token without a network read.
        Ok(vec![VersionEntry {
            token: "latest".to_string(),
            series: "smartos".to_string(),
            version: "latest".to_string(),
            title: "SmartOS (latest release)".to_string(),
            eol_date: None,
            supported: true,
            lts: false,
            dev: None,
            channel: true,
        }])
    }

    async fn resolve_release(
        &self,
        release: &str,
        http: &reqwest::Client,
    ) -> Result<ResolvedImage> {
        let resolved = releases::resolve(http, release).await?;
        let url: Url = resolved.url.parse().context("smartos image url")?;

        Ok(ResolvedImage {
            url,
            format: SourceFormat::RawGz,
            os: "illumos".to_string(),
            // Rolling release with no codename or major; flat
            // `smartos` series + the timestamp as version.
            series: "smartos".to_string(),
            version: resolved.release.clone(),
            description: format!(
                "SmartOS {} USB image (does NOT support cloud-init NoCloud; \
                 SmartOS uses mdata-get for guest metadata). Built to run \
                 on bhyve virtual machines.",
                resolved.release
            ),
            homepage: Url::parse("https://smartos.org/").context("smartos homepage url")?,
            ssh_key: false,
            verifier: Box::new(Sha256Pinned::from_document(
                resolved.sha256.clone(),
                checksum_document(
                    &resolved.checksum_url,
                    &resolved.checksum_filename,
                    SumsStyle::Gnu,
                )?,
            )),
            expected_sha256: Some(resolved.sha256),
            facts: ImageFacts {
                release: Some("rolling".to_string()),
                ..ImageFacts::default()
            },
        })
    }
}
