// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Per-vendor discovery of upstream cloud images.
//!
//! Carried over from a copy in another Triton repository (itself lifted
//! from `tritonadm image fetch-nocloud` in monitor-reef). Only the
//! discovery half lives here: the vendor profiles ([`vendor`]) and the
//! checksum strategies they hand back ([`verify`]). The fetch/convert
//! pipeline and IMGAPI manifest builder are client concerns and are not
//! part of this crate.
//!
//! ## HTTP client
//!
//! Every profile takes the caller's `reqwest::Client`, which must set a
//! User-Agent identifying the caller: GitHub's API (Talos, OpenBSD) and
//! `cloud.centos.org` (behind CloudFront) refuse requests without one.
//! Profiles do not set their own, so vendors see who is really asking.
//!
//! ## Surface
//!
//! - [`vendor::Vendor`] — built-in vendor enum (alma, ubuntu, …).
//! - [`vendor::all_vendors`] — the display catalog of every built-in
//!   vendor ([`vendor::VendorInfo`]).
//! - [`vendor::VendorProfile::list_versions`] — per-vendor catalog of
//!   resolvable releases ([`vendor::VersionEntry`]).
//! - [`vendor::VendorProfile::resolve`] — release-token → concrete
//!   [`vendor::ResolvedImage`].
//! - [`verify::Verifier`] — checksum strategies (Sha256Pinned,
//!   Sha256SumsTls, …) abstracted behind an async trait; a
//!   `ResolvedImage` carries one.

pub mod vendor;
pub mod verify;

pub use vendor::{
    ImageFacts, ResolvedImage, SignatureKind, SignatureRef, SourceFormat, Vendor, VendorInfo,
    VendorProfile, VersionEntry, all_vendors, lookup, validate_version_token,
};

/// Render a serde-Serialize enum to its serde-rename string form
/// (typically kebab-case via `#[serde(rename_all = "kebab-case")]`).
/// Inlined from the upstream tritonadm helper so the lift didn't
/// need to drag in any of tritonadm's CLI scaffolding.
pub fn enum_to_display<T: serde::Serialize + std::fmt::Debug>(val: &T) -> String {
    serde_json::to_value(val)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_else(|| format!("{val:?}"))
}
