// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! AlmaLinux cloud-image vendor profile.
//!
//! AlmaLinux publishes GenericCloud qcow2 images per major at
//! `https://repo.almalinux.org/almalinux/<major>/cloud/x86_64/images/`.
//! The directory listing at `/almalinux/` enumerates supported
//! majors (8, 9, 10, …); the `-latest.x86_64.qcow2` filename is a
//! rolling pointer to whatever build is current. The sibling
//! `CHECKSUM` file is Linux-style (`<sha256>  <filename>`), and
//! since the latest pointer and its dated alias share a hash we
//! can resolve the dated form once at metadata time and verify
//! with a plain `Sha256Pinned`.

mod releases;

use anyhow::{Context, Result};
use async_trait::async_trait;
use url::Url;

use super::{
    PinnedQcow2, ResolvedImage, VendorProfile, VersionEntry, checksum_document, detached_signature,
    point_release_of,
};
use crate::verify::SumsStyle;

pub struct Alma;

/// One entry per published major, newest first.
fn catalog(mut majors: Vec<u32>) -> Vec<VersionEntry> {
    majors.sort_unstable_by(|a, b| b.cmp(a));
    majors
        .into_iter()
        .map(|m| VersionEntry {
            token: m.to_string(),
            series: format!("alma{m}"),
            version: m.to_string(),
            title: format!("AlmaLinux {m}"),
            eol_date: None,
            supported: true,
            lts: false,
            dev: None,
            channel: false,
        })
        .collect()
}

#[async_trait]
impl VendorProfile for Alma {
    fn name(&self) -> &str {
        "alma"
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
        let url: Url = resolved.url.parse().context("alma image url")?;
        let point_release = point_release_of(&resolved.build);
        PinnedQcow2 {
            url,
            series: format!("alma{}", resolved.major),
            // Build identifier (e.g. `9.7-20260501`) so distinct
            // rebuilds dedupe in the manifest.
            version: resolved.build,
            description: format!(
                "AlmaLinux {} CloudInit NoCloud compatible image. \
                 Built to run on bhyve virtual machines.",
                resolved.major
            ),
            homepage: "https://almalinux.org/",
            document: checksum_document(
                &resolved.checksum_url,
                &resolved.checksum_filename,
                SumsStyle::Gnu,
            )?,
            point_release,
            signatures: vec![detached_signature(&resolved.checksum_url, ".asc")?],
            sha256: resolved.sha256,
        }
        .into_resolved("alma")
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
