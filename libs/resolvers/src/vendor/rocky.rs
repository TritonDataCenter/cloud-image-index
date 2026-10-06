// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Rocky Linux cloud-image vendor profile.
//!
//! Rocky publishes GenericCloud-Base qcow2 images per major at
//! `https://download.rockylinux.org/pub/rocky/<major>/images/x86_64/`,
//! with a per-file BSD-style `.CHECKSUM` sidecar
//! (`SHA256 (filename) = hex`). The release-resolution path fetches
//! both the directory listing and the chosen build's sidecar, so the
//! upstream sha256 is already known by the time the verifier runs —
//! we use a plain `Sha256Pinned` and callers know the hash without
//! downloading the qcow2.

mod releases;

use anyhow::{Context, Result};
use async_trait::async_trait;
use url::Url;

use super::{
    PinnedQcow2, ResolvedImage, VendorProfile, VersionEntry, checksum_document, detached_signature,
    point_release_of,
};
use crate::verify::SumsStyle;

pub struct Rocky;

/// One entry per published major, newest first.
fn catalog(mut majors: Vec<u32>) -> Vec<VersionEntry> {
    majors.sort_unstable_by(|a, b| b.cmp(a));
    majors
        .into_iter()
        .map(|m| VersionEntry {
            token: m.to_string(),
            series: format!("rocky{m}"),
            version: m.to_string(),
            title: format!("Rocky Linux {m}"),
            eol_date: None,
            supported: true,
            lts: false,
            dev: None,
            channel: false,
        })
        .collect()
}

#[async_trait]
impl VendorProfile for Rocky {
    fn name(&self) -> &str {
        "rocky"
    }

    async fn list_versions(&self, http: &reqwest::Client) -> Result<Vec<VersionEntry>> {
        Ok(catalog(releases::list(http).await?))
    }

    async fn resolve_release(
        &self,
        release: &str,
        http: &reqwest::Client,
    ) -> Result<ResolvedImage> {
        let resolved = releases::resolve(http, release).await?;
        let url: Url = resolved.url.parse().context("rocky image url")?;
        let point_release = point_release_of(&resolved.build);
        PinnedQcow2 {
            url,
            series: format!("rocky{}", resolved.major),
            version: resolved.build,
            description: format!(
                "Rocky Linux {} CloudInit NoCloud compatible image. \
                 Built to run on bhyve virtual machines.",
                resolved.major
            ),
            homepage: "https://rockylinux.org/",
            document: checksum_document(
                &resolved.checksum_url,
                &resolved.checksum_filename,
                SumsStyle::Bsd,
            )?,
            point_release,
            signatures: vec![detached_signature(&resolved.checksum_url, ".asc")?],
            sha256: resolved.sha256,
        }
        .into_resolved("rocky")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_newest_first() {
        let tokens: Vec<String> = catalog(vec![8, 10, 9])
            .into_iter()
            .map(|e| e.token)
            .collect();
        assert_eq!(tokens, ["10", "9", "8"]);
    }
}
