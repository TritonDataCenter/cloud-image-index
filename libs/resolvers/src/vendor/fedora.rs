// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Fedora cloud-image vendor profile.
//!
//! Fedora publishes Cloud_Base qcow2 images at
//! `https://download.fedoraproject.org/pub/fedora/linux/releases/<n>/Cloud/x86_64/images/`.
//! Release discovery uses `https://fedoraproject.org/releases.json`,
//! which lists every shipping artifact (variant × subvariant × arch ×
//! format) with the upstream sha256 inline — same shape as Ubuntu
//! Simple Streams, so we use a plain `Sha256Pinned` verifier.

mod releases;

use anyhow::{Context, Result};
use async_trait::async_trait;
use url::Url;

use super::{
    PinnedQcow2, ResolvedImage, SignatureKind, SignatureRef, VendorProfile, VersionEntry,
    checksum_document,
};
use crate::verify::SumsStyle;

pub struct Fedora;

/// Fedora's per-build checksum file, `Fedora-Cloud-<build>-x86_64-CHECKSUM`
/// beside the image: a clear-signed BSD-style listing that includes the
/// image's sha256.
fn checksum_file_signature(image_url: &str, build: &str) -> Result<SignatureRef> {
    let dir = image_url
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .ok_or_else(|| anyhow::anyhow!("fedora image url without a path: {image_url}"))?;
    let url = Url::parse(&format!("{dir}/Fedora-Cloud-{build}-x86_64-CHECKSUM"))
        .context("fedora CHECKSUM url")?;
    Ok(SignatureRef {
        url: url.clone(),
        kind: SignatureKind::Clearsigned,
        signs: url,
    })
}

#[async_trait]
impl VendorProfile for Fedora {
    fn name(&self) -> &str {
        "fedora"
    }

    async fn list_versions(&self, http: &reqwest::Client) -> Result<Vec<VersionEntry>> {
        let entries = releases::fetch(http).await?;
        let mut lifecycles = std::collections::BTreeMap::new();
        for version in releases::release_versions(&entries) {
            let life = releases::fetch_lifecycle(http, &version).await?;
            lifecycles.insert(version, life);
        }
        releases::catalog(&entries, &lifecycles)
    }

    async fn resolve_release(
        &self,
        release: &str,
        http: &reqwest::Client,
    ) -> Result<ResolvedImage> {
        let entries = releases::fetch(http).await?;
        let resolved = releases::resolve(&entries, release)?;
        let url: Url = resolved.url.parse().context("fedora image url")?;
        let signatures = vec![checksum_file_signature(url.as_str(), &resolved.build)?];
        PinnedQcow2 {
            url,
            // Fedora has no codenames after F20 — major is the
            // canonical short name used everywhere in the ecosystem.
            series: format!("f{}", releases::token_for(&resolved.major)),
            // Build serial (e.g. `44-1.7`) so distinct rebuilds of
            // the same major don't collide in the output filenames.
            version: resolved.build,
            description: format!(
                "Fedora {} Cloud Base CloudInit NoCloud compatible image. \
                 Built to run on bhyve virtual machines.",
                resolved.major
            ),
            homepage: "https://fedoraproject.org/",
            document: checksum_document(
                &resolved.checksum_url,
                &resolved.checksum_filename,
                SumsStyle::VendorDocument,
            )?,
            point_release: None,
            signatures,
            sha256: resolved.sha256,
        }
        .into_resolved("fedora")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_file_sits_beside_the_image() {
        let sig = checksum_file_signature(
            "https://download.fedoraproject.org/pub/fedora/linux/releases/44/Cloud/x86_64/images/Fedora-Cloud-Base-Generic-44-1.7.x86_64.qcow2",
            "44-1.7",
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            sig.url.as_str(),
            "https://download.fedoraproject.org/pub/fedora/linux/releases/44/Cloud/x86_64/images/Fedora-Cloud-44-1.7-x86_64-CHECKSUM"
        );
        assert_eq!(sig.kind, SignatureKind::Clearsigned);
        assert_eq!(sig.signs, sig.url);
    }
}
