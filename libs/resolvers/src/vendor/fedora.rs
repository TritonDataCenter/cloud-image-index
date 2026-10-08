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
//! format) with the upstream sha256 inline. The image is checked against
//! the build's clear-signed `CHECKSUM` file beside it instead, a generic
//! BSD-style listing that clients can read back from the index's
//! checksum source.

mod releases;

use anyhow::{Context, Result};
use async_trait::async_trait;
use url::Url;

use super::{
    PinnedQcow2, ResolvedImage, SignatureKind, SignatureRef, VendorProfile, VersionEntry,
    checksum_document,
};
use crate::verify::{Sha256BsdSumsTls, SumsStyle};

pub struct Fedora;

/// Fedora's per-build checksum file beside the image: a clear-signed
/// BSD-style listing that includes the image's sha256. Releases name it
/// `Fedora-Cloud-<build>-x86_64-CHECKSUM`; pre-releases, whose build
/// ids carry the label after an underscore (`45_Beta-1.3`),
/// `Fedora-Cloud-images-<build>-x86_64-CHECKSUM`.
fn checksum_file_url(image_url: &str, build: &str) -> Result<Url> {
    let dir = image_url
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .ok_or_else(|| anyhow::anyhow!("fedora image url without a path: {image_url}"))?;
    let prefix = if build.contains('_') {
        "Fedora-Cloud-images"
    } else {
        "Fedora-Cloud"
    };
    Url::parse(&format!("{dir}/{prefix}-{build}-x86_64-CHECKSUM")).context("fedora CHECKSUM url")
}

/// The checksum file is clear-signed, so it is also the signature.
fn checksum_file_signature(image_url: &str, build: &str) -> Result<SignatureRef> {
    let url = checksum_file_url(image_url, build)?;
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
        resolved_image(releases::resolve(&entries, release)?)
    }
}

/// The image for a release releases.json resolved to. Its hash is the
/// one in the build's CHECKSUM file (a generic BSD-style listing clients
/// can read back), not the one in releases.json.
fn resolved_image(resolved: releases::Resolved) -> Result<ResolvedImage> {
    let url: Url = resolved.url.parse().context("fedora image url")?;
    let checksum_url = checksum_file_url(url.as_str(), &resolved.build)?;
    let filename = url
        .path_segments()
        .and_then(|mut s| s.next_back())
        .unwrap_or_default()
        .to_string();
    let signatures = vec![checksum_file_signature(url.as_str(), &resolved.build)?];
    let mut image = PinnedQcow2 {
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
        document: checksum_document(checksum_url.as_str(), &filename, SumsStyle::Bsd)?,
        point_release: None,
        signatures,
        sha256: resolved.sha256,
    }
    .into_resolved("fedora")?;
    image.verifier = Box::new(Sha256BsdSumsTls::new(checksum_url, filename));
    Ok(image)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pre-releases name their checksum file differently (directory
    /// listings of 43, 44 and 45_Beta, 2026-10-08).
    #[test]
    fn a_pre_release_checksum_file_has_images_in_its_name() {
        let url = checksum_file_url(
            "https://download.fedoraproject.org/pub/fedora/linux/releases/test/45_Beta/Cloud/x86_64/images/Fedora-Cloud-Base-Generic-45_Beta-1.3.x86_64.qcow2",
            "45_Beta-1.3",
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            url.as_str(),
            "https://download.fedoraproject.org/pub/fedora/linux/releases/test/45_Beta/Cloud/x86_64/images/Fedora-Cloud-images-45_Beta-1.3-x86_64-CHECKSUM"
        );
    }

    /// The published digest must come from the generic CHECKSUM file
    /// beside the image, not from releases.json.
    #[test]
    fn the_checksum_source_is_the_builds_checksum_file() {
        let resolved = releases::Resolved {
            major: "44".to_string(),
            build: "44-1.7".to_string(),
            url: "https://download.fedoraproject.org/pub/fedora/linux/releases/44/Cloud/x86_64/images/Fedora-Cloud-Base-Generic-44-1.7.x86_64.qcow2".to_string(),
            sha256: "a".repeat(64),
        };
        let image = resolved_image(resolved).unwrap_or_else(|e| panic!("{e:#}"));
        assert_eq!(
            image.verifier.checksum_source(),
            crate::verify::ChecksumSource::Document {
                url: Url::parse("https://download.fedoraproject.org/pub/fedora/linux/releases/44/Cloud/x86_64/images/Fedora-Cloud-44-1.7-x86_64-CHECKSUM")
                    .unwrap_or_else(|e| panic!("{e}")),
                filename: "Fedora-Cloud-Base-Generic-44-1.7.x86_64.qcow2".to_string(),
                style: SumsStyle::Bsd,
                algorithm: crate::verify::HashAlgorithm::Sha256,
            }
        );
        assert_eq!(
            image.expected_sha256.as_deref(),
            Some("a".repeat(64).as_str())
        );
    }

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
