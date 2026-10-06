// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! FreeBSD cloud-image vendor profile.
//!
//! FreeBSD publishes the BASIC-CLOUDINIT VM images at
//! `https://download.freebsd.org/releases/VM-IMAGES/<ver>-RELEASE/amd64/Latest/`,
//! one per release. The image is `.raw.xz`, which a client can
//! stream-decompress straight onto the target disk. The sibling `CHECKSUM.SHA256` is in
//! BSD-traditional `SHA256 (filename) = hex` format, which we
//! handle via `Sha256BsdSumsTls`.

mod releases;

use anyhow::{Context, Result};
use async_trait::async_trait;
use url::Url;

use super::{ResolvedImage, SourceFormat, VendorProfile, VersionEntry};
use crate::verify::Sha256BsdSumsTls;

pub struct FreeBsd;

#[async_trait]
impl VendorProfile for FreeBsd {
    fn name(&self) -> &str {
        "freebsd"
    }

    async fn list_versions(&self, http: &reqwest::Client) -> Result<Vec<VersionEntry>> {
        Ok(releases::list(http)
            .await?
            .into_iter()
            .map(|v| {
                let major = v.split('.').next().unwrap_or(&v).to_string();
                VersionEntry {
                    token: v.clone(),
                    series: major,
                    version: v.clone(),
                    title: format!("FreeBSD {v}-RELEASE"),
                    eol_date: None,
                    supported: true,
                    lts: false,
                    dev: None,
                    channel: false,
                }
            })
            .collect())
    }

    async fn resolve_release(
        &self,
        release: &str,
        http: &reqwest::Client,
    ) -> Result<ResolvedImage> {
        let version = if release.trim() == "latest" {
            releases::find_latest(http).await?
        } else {
            releases::parse_version(release)?
        };

        // Major-only series ("15") rather than "15.0" so multiple
        // FreeBSD point releases share a manifest name and dedupe
        // sensibly. The version field carries the full "15.0".
        let major = version
            .split('.')
            .next()
            .ok_or_else(|| anyhow::anyhow!("freebsd: version missing major: {version}"))?
            .to_string();

        let filename = format!("FreeBSD-{version}-RELEASE-amd64-BASIC-CLOUDINIT-zfs.raw.xz");
        let base = format!(
            "https://download.freebsd.org/releases/VM-IMAGES/{version}-RELEASE/amd64/Latest/"
        );
        let url: Url = format!("{base}{filename}")
            .parse()
            .context("freebsd image url")?;
        let sums_url: Url = format!("{base}CHECKSUM.SHA256")
            .parse()
            .context("freebsd CHECKSUM.SHA256 url")?;

        Ok(ResolvedImage {
            url,
            format: SourceFormat::Xz,
            os: "bsd".to_string(),
            series: major,
            version: version.clone(),
            description: format!(
                "FreeBSD {version}-RELEASE CloudInit NoCloud compatible image. \
                 Built to run on bhyve virtual machines."
            ),
            homepage: Url::parse("https://www.freebsd.org/").context("freebsd homepage url")?,
            ssh_key: true,
            verifier: Box::new(Sha256BsdSumsTls::new(sums_url, filename)),
            // BSD-style CHECKSUM.SHA256 is fetched at verify time,
            // so we don't have the upstream sha256 pre-download.
            expected_sha256: None,
            facts: Default::default(),
        })
    }
}
