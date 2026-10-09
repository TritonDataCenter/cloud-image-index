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
//! serial, the exact item URL, and the sha256 in one TLS roundtrip. The
//! hash the index publishes comes from the release directory's
//! `SHA256SUMS`, a generic listing clients can read back.
//! A streams failure is an error: the feed is served from the same host
//! as the images, so there is no useful fallback, and a format change
//! should fail loudly rather than degrade to a rolling `current/` URL.

mod streams;

use anyhow::{Context, Result};
use async_trait::async_trait;
use url::Url;

use super::{
    ImageFacts, ResolvedImage, SourceFormat, VendorProfile, VersionEntry, detached_signature,
};
use crate::verify::Sha256SumsTls;

/// The Ubuntu profile. It keeps the streams feed (about 18 MB) once
/// fetched, so listing versions and resolving each release read one
/// copy instead of fetching it each time. A failed fetch is not kept.
#[derive(Default)]
pub struct Ubuntu {
    streams: tokio::sync::OnceCell<streams::Streams>,
}

impl Ubuntu {
    #[cfg(test)]
    fn with_streams(streams: streams::Streams) -> Self {
        Ubuntu {
            streams: tokio::sync::OnceCell::new_with(Some(streams)),
        }
    }

    async fn streams(&self, http: &reqwest::Client) -> Result<&streams::Streams> {
        self.streams.get_or_try_init(|| streams::fetch(http)).await
    }
}

#[async_trait]
impl VendorProfile for Ubuntu {
    fn name(&self) -> &str {
        "ubuntu"
    }

    async fn list_versions(&self, http: &reqwest::Client) -> Result<Vec<VersionEntry>> {
        Ok(streams::catalog(self.streams(http).await?))
    }

    async fn resolve_release(
        &self,
        release: &str,
        http: &reqwest::Client,
    ) -> Result<ResolvedImage> {
        resolve_via_streams(self.streams(http).await?, release)
    }
}

fn resolve_via_streams(index: &streams::Streams, release: &str) -> Result<ResolvedImage> {
    let img = streams::resolve(index, release)?;

    let description = format!(
        "Ubuntu {} ({}) CloudInit NoCloud compatible image. \
         Built to run on bhyve virtual machines.",
        img.release_title, img.codename
    );

    // Canonical signs the release directory's SHA256SUMS, which lists
    // the same hash the streams feed gives. The index publishes the one
    // in SHA256SUMS, a generic listing clients can read back; the feed
    // only says which image is current.
    let sums_url = img
        .url
        .as_str()
        .rsplit_once('/')
        .map(|(dir, _)| format!("{dir}/SHA256SUMS"))
        .ok_or_else(|| anyhow::anyhow!("ubuntu image url without a path: {}", img.url))?;
    let filename = img
        .url
        .path_segments()
        .and_then(|mut s| s.next_back())
        .unwrap_or_default()
        .to_string();

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
        verifier: Box::new(Sha256SumsTls::new(
            Url::parse(&sums_url).context("ubuntu SHA256SUMS url")?,
            filename,
        )),
        expected_sha256: Some(img.sha256),
        facts: ImageFacts {
            signatures: vec![detached_signature(&sums_url, ".gpg")?],
            ..ImageFacts::default()
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The published digest must come from the generic SHA256SUMS file
    /// beside the image, which a client can read back, not from the
    /// streams feed, which only vendor-specific code can.
    #[test]
    fn the_checksum_source_is_the_release_directorys_sha256sums() {
        let image = resolve_via_streams(&streams::tests::fixture(), "noble")
            .unwrap_or_else(|e| panic!("{e:#}"));
        let filename = image
            .url
            .path_segments()
            .and_then(|mut s| s.next_back())
            .unwrap_or_default()
            .to_string();
        let sums = image
            .url
            .join("SHA256SUMS")
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            image.verifier.checksum_source(),
            crate::verify::ChecksumSource::Document {
                url: sums,
                filename,
                style: crate::verify::SumsStyle::Gnu,
                algorithm: crate::verify::HashAlgorithm::Sha256,
            }
        );
    }

    /// A client whose every request fails at once, so a test passes only
    /// if nothing is fetched.
    fn offline() -> reqwest::Client {
        crate::test_client_builder()
            .proxy(reqwest::Proxy::all("http://127.0.0.1:9").unwrap_or_else(|e| panic!("{e}")))
            .build()
            .unwrap_or_else(|e| panic!("{e}"))
    }

    #[tokio::test]
    async fn listing_and_resolving_read_the_feed_the_profile_already_has() {
        // The feed is 18 MB; one run lists versions and resolves every
        // release, and must not fetch it for each.
        let ubuntu = Ubuntu::with_streams(streams::tests::fixture());
        let http = offline();
        let versions = ubuntu
            .list_versions(&http)
            .await
            .unwrap_or_else(|e| panic!("{e:#}"));
        assert!(!versions.is_empty());
        let image = ubuntu
            .resolve_release("noble", &http)
            .await
            .unwrap_or_else(|e| panic!("{e:#}"));
        assert_eq!(image.version, "20260301");
    }

    #[tokio::test]
    async fn a_profile_without_the_feed_fetches_it() {
        let err = Ubuntu::default().list_versions(&offline()).await.err();
        assert!(err.is_some(), "the offline client must have been used");
    }
}
