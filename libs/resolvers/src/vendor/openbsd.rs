// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! OpenBSD cloud-image vendor profile.
//!
//! Uses the bsd-cloud-image.org-blessed builds at
//! `hcartiaux/openbsd-cloud-image` GitHub releases. Resolves
//! either `latest` (via the GitHub Releases API) or an explicit
//! `<X>.<Y>` (e.g. `7.8`) by walking the release list. The qcow2
//! and its `.sha256` sidecar are both release assets; we pre-fetch
//! the sidecar at metadata time and pin the hash.

mod releases;

use anyhow::{Context, Result};
use async_trait::async_trait;
use url::Url;

use super::{
    ImageFacts, ResolvedImage, SourceFormat, VendorProfile, VersionEntry, checksum_document,
};
use crate::verify::{Sha256Pinned, SumsStyle};

pub struct OpenBsd;

#[async_trait]
impl VendorProfile for OpenBsd {
    fn name(&self) -> &str {
        "openbsd"
    }

    async fn list_versions(&self, _http: &reqwest::Client) -> Result<Vec<VersionEntry>> {
        // Resolution walks the `hcartiaux/openbsd-cloud-image` GitHub
        // releases; the catalog exposes the
        // `latest` pointer the resolver follows without hitting the
        // GitHub API here. An explicit `X.Y` token still resolves.
        Ok(vec![VersionEntry {
            token: "latest".to_string(),
            series: "latest".to_string(),
            version: "latest".to_string(),
            title: "OpenBSD (latest release)".to_string(),
            eol_date: None,
            supported: true,
            lts: false,
            dev: None,
            channel: true,
        }])
    }

    async fn resolve_release(
        &self,
        release: &str,
        http: &reqwest::Client,
    ) -> Result<ResolvedImage> {
        let resolved = releases::resolve(http, release).await?;
        let url: Url = resolved.url.parse().context("openbsd image url")?;

        Ok(ResolvedImage {
            url,
            format: SourceFormat::Qcow2,
            os: "bsd".to_string(),
            series: resolved.version.clone(),
            // Use the full upstream tag as the manifest version so
            // distinct rebuilds of the same `<X>.<Y>` (different
            // datestamp on the tag) dedupe sensibly.
            version: resolved.tag.clone(),
            description: format!(
                "OpenBSD {} CloudInit NoCloud compatible image (bsd-cloud-image.org build). \
                 Built to run on bhyve virtual machines.",
                resolved.version
            ),
            homepage: Url::parse("https://bsd-cloud-image.org/").context("openbsd homepage url")?,
            ssh_key: true,
            verifier: Box::new(Sha256Pinned::from_document(
                resolved.sha256.clone(),
                checksum_document(&resolved.checksum_url, "", SumsStyle::Bare)?,
            )),
            expected_sha256: Some(resolved.sha256),
            facts: ImageFacts {
                release: Some(resolved.version.clone()),
                published_at: resolved.published_at,
                ..ImageFacts::default()
            },
        })
    }
}
