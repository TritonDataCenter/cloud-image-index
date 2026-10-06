// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Alpine Linux cloud-image vendor profile.
//!
//! Alpine publishes nocloud-capable cloud images at
//! `https://dl-cdn.alpinelinux.org/alpine/v<branch>/releases/cloud/`
//! (e.g. `alpine-3.24.2-x86_64-cloudinit-r0.qcow2`; before 3.24,
//! `nocloud_alpine-3.23.4-x86_64-uefi-cloudinit-r0.qcow2`), each with a
//! sibling `.sha512` sidecar containing only the bare hex hash. Cloud
//! images are not built for every point release, so the image is picked
//! from the directory listing rather than derived from the newest
//! release. Branch discovery and EOL dates come from `releases.json`
//! on alpinelinux.org.

mod releases;

use anyhow::{Context, Result};
use async_trait::async_trait;
use url::Url;

use super::{
    ImageFacts, ResolvedImage, SourceFormat, VendorProfile, VersionEntry, detached_signature,
};
use crate::verify::Sha512SidecarTls;

pub struct Alpine;

#[async_trait]
impl VendorProfile for Alpine {
    fn name(&self) -> &str {
        "alpine"
    }

    async fn list_versions(&self, http: &reqwest::Client) -> Result<Vec<VersionEntry>> {
        let rj = releases::fetch(http)
            .await
            .with_context(|| "fetch alpine releases.json")?;
        Ok(releases::catalog(&rj, chrono::Utc::now().date_naive()))
    }

    async fn resolve_release(
        &self,
        release: &str,
        http: &reqwest::Client,
    ) -> Result<ResolvedImage> {
        let rj = releases::fetch(http)
            .await
            .with_context(|| "fetch alpine releases.json")?;
        let resolved = releases::resolve(&rj, release)?;

        // resolved.branch is "v3.23"; we use it as the URL path
        // component. The series field of the manifest gets the
        // version-only form ("3.23") so we don't double up the `v`.
        let series = resolved
            .branch
            .strip_prefix('v')
            .unwrap_or(&resolved.branch)
            .to_string();
        let base = format!(
            "https://dl-cdn.alpinelinux.org/alpine/{}/releases/cloud/",
            resolved.branch
        );
        let listing = http
            .get(&base)
            .send()
            .await
            .with_context(|| format!("GET {base}"))?
            .error_for_status()
            .with_context(|| format!("status from {base}"))?
            .text()
            .await
            .with_context(|| format!("read body of {base}"))?;
        // A full point-release token asks for that exact build; a
        // branch or `latest` takes the newest image the branch has.
        let want = (release.matches('.').count() == 2).then_some(resolved.version.as_str());
        let image = releases::pick_cloud_image(&listing, want)
            .ok_or_else(|| anyhow::anyhow!("alpine: no x86_64 cloud-init qcow2 under {base}"))?;
        let version = image.version;
        let filename = image.filename;
        let url: Url = format!("{base}{filename}")
            .parse()
            .context("alpine image url")?;
        let sidecar_url: Url = format!("{base}{filename}.sha512")
            .parse()
            .context("alpine sha512 sidecar url")?;

        let signatures = vec![detached_signature(url.as_str(), ".asc")?];
        Ok(ResolvedImage {
            url,
            format: SourceFormat::Qcow2,
            os: "linux".to_string(),
            series: series.clone(),
            version: version.clone(),
            description: format!(
                "Alpine Linux v{version} CloudInit NoCloud compatible image. \
                 Built to run on bhyve virtual machines."
            ),
            homepage: Url::parse("https://alpinelinux.org/").context("alpine homepage url")?,
            ssh_key: true,
            verifier: Box::new(Sha512SidecarTls { sidecar_url }),
            // Hash channel is SHA-512 sidecar, so no sha256 is known
            // before download.
            expected_sha256: None,
            facts: ImageFacts {
                point_release: Some(version.clone()),
                signatures,
                ..ImageFacts::default()
            },
        })
    }
}
