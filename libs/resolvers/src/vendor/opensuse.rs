// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! openSUSE Leap cloud-image vendor profile.
//!
//! Leap publishes per-version Minimal-VM Cloud qcow2 images at
//! `https://download.opensuse.org/distribution/leap/<X>.<Y>/appliances/`.
//! Versions and their lifecycle state come from openSUSE's
//! `get.opensuse.org/api/v0/distributions.json`; the image in a
//! version's directory is found via MirrorCache's JSON listing
//! (`?json=1`). The sidecar is a Linux-style `.sha256` sibling so
//! we pin the hash at metadata time. Tumbleweed is intentionally skipped — its
//! current `appliances/` directory only ships MicroOS-flavored
//! immutable images (Combustion/Ignition, not cloud-init nocloud).

mod releases;

use anyhow::{Context, Result};
use async_trait::async_trait;
use url::Url;

use super::{PinnedQcow2, ResolvedImage, VendorProfile, VersionEntry, checksum_document};
use crate::verify::SumsStyle;

pub struct OpenSuse;

#[async_trait]
impl VendorProfile for OpenSuse {
    fn name(&self) -> &str {
        "opensuse"
    }

    async fn list_versions(&self, http: &reqwest::Client) -> Result<Vec<VersionEntry>> {
        Ok(releases::catalog(
            &releases::fetch_distributions(http).await?,
        ))
    }

    async fn resolve_release(
        &self,
        release: &str,
        http: &reqwest::Client,
    ) -> Result<ResolvedImage> {
        let resolved = releases::resolve(http, release).await?;
        let url: Url = resolved.url.parse().context("opensuse image url")?;
        PinnedQcow2 {
            url,
            series: format!("leap{}", resolved.leap_version),
            version: resolved.build,
            description: format!(
                "openSUSE Leap {} CloudInit NoCloud compatible image. \
                 Built to run on bhyve virtual machines.",
                resolved.leap_version
            ),
            homepage: "https://www.opensuse.org/",
            document: checksum_document(
                &resolved.checksum_url,
                &resolved.checksum_filename,
                SumsStyle::Gnu,
            )?,
            point_release: None,
            signatures: Vec::new(),
            sha256: resolved.sha256,
        }
        .into_resolved("opensuse")
    }
}
