// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! CentOS Stream cloud-image vendor profile.
//!
//! CentOS Stream publishes GenericCloud qcow2 images per stream at
//! `https://cloud.centos.org/centos/<n>-stream/x86_64/images/`,
//! with a per-file BSD-style `<filename>.SHA256SUM` sidecar. We
//! list the directory, pick the highest-versioned dated build,
//! pre-fetch the sidecar at resolve time, and pin the hash so callers
//! know it without downloading the image.

mod releases;

use anyhow::{Context, Result};
use async_trait::async_trait;
use url::Url;

use super::{PinnedQcow2, ResolvedImage, VendorProfile, VersionEntry, checksum_document};
use crate::verify::SumsStyle;

pub struct CentosStream;

/// One entry per published major, newest first.
fn catalog(mut majors: Vec<u32>) -> Vec<VersionEntry> {
    majors.sort_unstable_by(|a, b| b.cmp(a));
    majors
        .into_iter()
        .map(|s| VersionEntry {
            token: s.to_string(),
            series: format!("centos{s}"),
            version: s.to_string(),
            title: format!("CentOS Stream {s}"),
            eol_date: None,
            supported: s >= 9,
            lts: false,
            dev: None,
            channel: false,
        })
        .collect()
}

#[async_trait]
impl VendorProfile for CentosStream {
    fn name(&self) -> &str {
        "centos-stream"
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
        let url: Url = resolved.url.parse().context("centos-stream image url")?;
        PinnedQcow2 {
            url,
            series: format!("centos{}", resolved.stream),
            version: resolved.build,
            description: format!(
                "CentOS Stream {} CloudInit NoCloud compatible image. \
                 Built to run on bhyve virtual machines.",
                resolved.stream
            ),
            homepage: "https://www.centos.org/",
            document: checksum_document(
                &resolved.checksum_url,
                &resolved.checksum_filename,
                SumsStyle::Bsd,
            )?,
            point_release: None,
            signatures: Vec::new(),
            sha256: resolved.sha256,
        }
        .into_resolved("centos-stream")
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
