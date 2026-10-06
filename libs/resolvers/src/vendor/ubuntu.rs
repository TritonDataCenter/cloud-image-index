// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Ubuntu cloud-image vendor profile.
//!
//! Resolves a release token (`latest`, `noble`, `24.04`, ...) to a
//! concrete cloud image. The primary path consults Canonical's Simple
//! Streams metadata feed, which gives us the canonical upstream build
//! serial, the exact item URL, and the sha256 in one TLS roundtrip.
//! A streams failure is an error: the feed is served from the same host
//! as the images, so there is no useful fallback, and a format change
//! should fail loudly rather than degrade to a rolling `current/` URL.

mod streams;

use anyhow::{Context, Result};
use async_trait::async_trait;
use url::Url;

use super::{
    ImageFacts, ResolvedImage, SourceFormat, VendorProfile, VersionEntry, checksum_document,
    detached_signature,
};
use crate::verify::{Sha256Pinned, SumsStyle};

pub struct Ubuntu;

#[async_trait]
impl VendorProfile for Ubuntu {
    fn name(&self) -> &str {
        "ubuntu"
    }

    async fn list_versions(&self, http: &reqwest::Client) -> Result<Vec<VersionEntry>> {
        Ok(streams::catalog(&streams::fetch(http).await?))
    }

    async fn resolve_release(
        &self,
        release: &str,
        http: &reqwest::Client,
    ) -> Result<ResolvedImage> {
        resolve_via_streams(release, http).await
    }
}

async fn resolve_via_streams(release: &str, http: &reqwest::Client) -> Result<ResolvedImage> {
    let index = streams::fetch(http).await?;
    let img = streams::resolve(&index, release)?;

    let description = format!(
        "Ubuntu {} ({}) CloudInit NoCloud compatible image. \
         Built to run on bhyve virtual machines.",
        img.release_title, img.codename
    );

    // Canonical signs the release directory's SHA256SUMS, which lists
    // the same hash the streams feed gives.
    let sums_url = img
        .url
        .as_str()
        .rsplit_once('/')
        .map(|(dir, _)| format!("{dir}/SHA256SUMS"))
        .ok_or_else(|| anyhow::anyhow!("ubuntu image url without a path: {}", img.url))?;

    Ok(ResolvedImage {
        url: img.url,
        format: SourceFormat::Qcow2,
        os: "linux".to_string(),
        series: img.codename,
        // Use the upstream build serial as the manifest version. Two
        // runs against the same upstream produce the same manifest
        // version, which matches what IMGAPI consumers expect.
        version: img.serial,
        description,
        homepage: Url::parse("https://ubuntu.com/").context("ubuntu homepage url")?,
        ssh_key: true,
        verifier: Box::new(Sha256Pinned::from_document(
            img.sha256.clone(),
            checksum_document(streams::STREAMS_URL, "", SumsStyle::VendorDocument)?,
        )),
        expected_sha256: Some(img.sha256),
        facts: ImageFacts {
            signatures: vec![detached_signature(&sums_url, ".gpg")?],
            ..ImageFacts::default()
        },
    })
}
