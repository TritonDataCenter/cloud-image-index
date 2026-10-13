// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The cloud-image-index format.
//!
//! The index is a tree of static JSON files served over plain HTTPS (in
//! the first iteration, GitHub Pages). This crate describes that tree as
//! a Dropshot API trait so that an OpenAPI document can be generated and
//! published, and so that a reference server can serve the same tree.
//! Every endpoint path is the path of a file; no path is both a file and
//! a directory. Dropshot does not allow a literal path segment and a
//! variable one at the same level, so literal names (`distros`,
//! `aliases`, `releases`, `index.json`) never sit beside `{distro}` or
//! `{release}`.
//!
//! The index is a directory, not a distributor: it describes what each
//! vendor publishes and where, including the vendor's own checksums and
//! signatures, and clients decide how far to trust it. See
//! `docs/design.md`.

pub mod tree;

use chrono::{DateTime, NaiveDate, Utc};
use dropshot::{Body, HttpError, HttpResponseOk, Path, RequestContext};
use schemars::JsonSchema;
use schemars::schema::{InstanceType, Metadata, Schema, SchemaObject};
use serde::{Deserialize, Serialize};

/// The schema for an open enum: a string, with the values this version
/// of the index defines listed in the description. It does not use
/// `enum` or a `pattern`, so generated clients take the value as a
/// string and accept, and keep, values added after they were generated;
/// and every OpenAPI tool can read it (a lookahead pattern excluding the
/// known values, as an earlier shape used, is beyond Go's RE2, which
/// rejected the whole document).
fn open_enum_schema(description: &str) -> Schema {
    Schema::Object(SchemaObject {
        metadata: Some(Box::new(Metadata {
            description: Some(description.to_string()),
            ..Metadata::default()
        })),
        instance_type: Some(InstanceType::String.into()),
        ..SchemaObject::default()
    })
}

/// Define an open enum: its documented variants, an `Other(String)`
/// catch-all for values a later version of the index may add, the list
/// of defined values (`KNOWN`), and its open schema. The variants and
/// the descriptions (the enum's doc comment, then each value with its
/// own doc comment) are written once, so the schema cannot drift from
/// the Rust type.
macro_rules! open_enum {
    (
        $(#[doc = $doc:expr])*
        pub enum $ty:ident {
            $( $(#[doc = $variant_doc:expr])* $variant:ident, )+
        }
    ) => {
        $(#[doc = $doc])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $ty {
            $( $(#[doc = $variant_doc])* $variant, )+
            /// A value this version of the index does not define. The
            /// index itself never publishes one; clients should accept it.
            #[serde(untagged)]
            Other(String),
        }

        impl $ty {
            /// The values this version of the index defines.
            pub const KNOWN: &'static [$ty] = &[$($ty::$variant),+];
        }

        impl JsonSchema for $ty {
            fn schema_name() -> String {
                stringify!($ty).to_string()
            }

            fn json_schema(_: &mut schemars::r#gen::SchemaGenerator) -> Schema {
                let join = |lines: &[&str]| {
                    lines.iter().map(|l| l.trim()).collect::<Vec<_>>().join(" ")
                };
                let mut description = join(&[$($doc),*]);
                description.push_str(
                    "\n\nLater versions of the index may add values; clients \
                     should accept and keep values they do not know.",
                );
                description.push_str("\n\nKnown values:\n");
                $(
                    let value = serde_json::to_value($ty::$variant)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_string))
                        .unwrap_or_default();
                    let doc = join(&[$($variant_doc),*]);
                    if doc.is_empty() {
                        description.push_str(&format!("\n- `{value}`"));
                    } else {
                        description.push_str(&format!("\n- `{value}`: {doc}"));
                    }
                )+
                open_enum_schema(&description)
            }
        }
    };
}

// ---------------------------------------------------------------------
// v1/index.json
// ---------------------------------------------------------------------

/// `v1/index.json`: every distro in the index.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DistroList {
    pub distros: Vec<DistroSummary>,
}

/// One distro as listed in `v1/index.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DistroSummary {
    /// Our short name for the distro, also its directory name
    /// (e.g. `ubuntu`, `rocky`, `centos-stream`).
    pub id: String,
    /// Human-facing name (e.g. `Rocky Linux`).
    pub name: Option<String>,
    pub os_family: OsFamily,
    pub homepage: Option<String>,
    /// When the index last read this distro from its vendor successfully
    /// (UTC). A run that fails for a vendor keeps its previous data and
    /// this time, so an old value means the vendor's data is stale.
    pub checked_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OsFamily {
    Linux,
    Bsd,
    Illumos,
}

// ---------------------------------------------------------------------
// v1/images.json
// ---------------------------------------------------------------------

/// `v1/images.json`: every current build in the index, one entry each,
/// so a client can list or search them with a single request. Entries
/// come in `v1/index.json`'s distro order, then each distro's releases
/// newest first, then each release's current builds newest first.
/// Archived builds are left out.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ImageList {
    pub images: Vec<Image>,
}

/// One current build, with the distro and release it belongs to.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Image {
    /// The distro, as `v1/index.json` lists it.
    pub distro: DistroSummary,
    /// The release the build belongs to.
    pub release: Release,
    pub build: Build,
}

/// The image list for an index: `list` is its `v1/index.json`, and each
/// distro comes with the build lists of its releases, in the same order
/// as `distro.releases`. A distro or release missing from `distros` is
/// left out; the tree checks report it.
pub fn image_list(list: &DistroList, distros: &[(Distro, Vec<BuildList>)]) -> ImageList {
    let mut images = Vec::new();
    for summary in &list.distros {
        let Some((distro, build_lists)) = distros.iter().find(|(d, _)| d.id == summary.id) else {
            continue;
        };
        for (release, build_list) in distro.releases.iter().zip(build_lists) {
            for build in &build_list.builds {
                images.push(Image {
                    distro: summary.clone(),
                    release: release.clone(),
                    build: build.clone(),
                });
            }
        }
    }
    ImageList { images }
}

// ---------------------------------------------------------------------
// v1/distros/<distro>/index.json
// ---------------------------------------------------------------------

/// `v1/distros/<distro>/index.json`: a distro and its releases, newest first.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Distro {
    pub id: String,
    pub name: Option<String>,
    pub os_family: OsFamily,
    pub homepage: Option<String>,
    /// When the index last read this distro from its vendor successfully
    /// (UTC). A run that fails for a vendor keeps its previous data and
    /// this time, so an old value means the vendor's data is stale.
    pub checked_at: Option<DateTime<Utc>>,
    /// The vendor's name for the channel the `dev` alias follows
    /// (e.g. `bloody`, `beta`, `rc`). Null when the vendor has no
    /// development channel in the index.
    pub dev_channel: Option<String>,
    pub releases: Vec<Release>,
}

/// One release of a distro. A release is usually the vendor's major
/// release or series (`noble`, `9`, `trixie`), with point releases
/// recorded on builds; FreeBSD lists each supported point release as a
/// release (`14.4`, `15.1`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Release {
    /// Release token, also the release's directory name (e.g. `noble`,
    /// `9`, `1.14`). For vendors that publish channels rather than
    /// releases it is what the channel resolved to (OmniOS `r151058`,
    /// Arch `rolling`).
    pub id: String,
    /// Vendor version string (e.g. `24.04`, `9`, `1.14`).
    pub version: Option<String>,
    /// Human-facing title (e.g. `24.04 LTS Noble Numbat`).
    pub title: Option<String>,
    /// Aliases this release currently holds. Each alias is held by at
    /// most one release of a distro.
    #[serde(default)]
    pub aliases: Vec<Alias>,
    /// The vendor's end-of-support date, when published.
    pub eol_date: Option<NaiveDate>,
    /// Matching osinfo-db entry, when one exists. Not yet populated: see
    /// <https://github.com/TritonDataCenter/cloud-image-index/blob/main/docs/design.md#osinfo-db-cross-references>.
    pub osinfo: Option<OsinfoRef>,
}

open_enum! {
    /// A release alias.
    pub enum Alias {
        /// The newest generally-available release. Never a pre-release.
        Latest,
        /// The newest release the vendor itself labels long-term
        /// support. Absent for vendors that do not use that label.
        Lts,
        /// The release the vendor's development channel currently
        /// resolves to.
        Dev,
    }
}

/// A cross-reference to an osinfo-db OS entry
/// (<https://gitlab.com/libosinfo/osinfo-db>).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct OsinfoRef {
    /// Full osinfo id (e.g. `http://ubuntu.com/ubuntu/24.04`).
    pub id: String,
    /// The entry's primary short id (e.g. `ubuntu24.04`), as accepted
    /// by `virt-install --osinfo`.
    pub short_id: String,
}

// ---------------------------------------------------------------------
// v1/distros/<distro>/releases/<release>/index.json and
// v1/distros/<distro>/aliases/*.json
// ---------------------------------------------------------------------

/// The builds of one release. Used for
/// `v1/distros/<distro>/releases/<release>/index.json` (builds in the
/// vendor's main tree) and the alias files under
/// `v1/distros/<distro>/aliases/`, which are a copy of the aliased
/// release's `index.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BuildList {
    pub distro: String,
    pub release: String,
    /// Newest first.
    pub builds: Vec<Build>,
}

/// One vendor build. A build is identified by
/// `distro / release / build`, never by its URL.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Build {
    /// The vendor's build id or serial (e.g. `20260927`,
    /// `9.8-20260525.0`, `1.14.2`).
    pub build: String,
    /// Vendor point release this build belongs to, for distros that
    /// have them (e.g. `9.8`).
    pub point_release: Option<String>,
    /// When the vendor published the build, if the vendor says.
    pub published_at: Option<DateTime<Utc>>,
    /// When the index first recorded this build.
    pub first_seen: Option<DateTime<Utc>>,
    /// Overrides the release's osinfo reference when osinfo-db has a
    /// more specific entry for this build's point release. Not yet
    /// populated (see the release's `osinfo`).
    pub osinfo: Option<OsinfoRef>,
    pub artifacts: Vec<Artifact>,
}

/// One downloadable image file of a build. Within a build an artifact
/// is identified by `variant / arch / format`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Artifact {
    /// The vendor's flavour of the image (e.g. `server`, `base`, `lvm`,
    /// `genericcloud`, `nocloud`).
    pub variant: Option<String>,
    /// Whether this is the vendor's default variant.
    pub default_variant: Option<bool>,
    /// Architecture, using osinfo-db names (`x86_64`, `aarch64`, ...).
    pub arch: String,
    pub format: ImageFormat,
    pub compression: Compression,
    /// Firmware the image is known to boot with. Empty when unknown.
    #[serde(default)]
    pub firmware: Vec<Firmware>,
    /// cloud-init datasources (or compatible implementations) the image
    /// is known to support, using cloud-init's lowercase names
    /// (e.g. `nocloud`). Empty when unknown.
    #[serde(default)]
    pub datasources: Vec<String>,
    /// Whether the image accepts SSH public keys from instance metadata.
    /// Absent means unknown, not true.
    pub ssh_key_injection: Option<bool>,
    /// Size in bytes of the file as downloaded, when known.
    pub size: Option<u64>,
    /// Where the file can be downloaded. Every build has a `primary`
    /// location.
    pub locations: Vec<Location>,
    pub integrity: Option<Integrity>,
}

open_enum! {
    /// Firmware an image boots with.
    pub enum Firmware {
        Bios,
        Uefi,
    }
}

open_enum! {
    /// Disk image format of the file, once decompressed.
    pub enum ImageFormat {
        Qcow2,
        Raw,
        Vmdk,
    }
}

open_enum! {
    /// Compression applied to the file as downloaded.
    pub enum Compression {
        None,
        Gzip,
        Xz,
        Zstd,
        Bzip2,
    }
}

/// A download location for an artifact.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Location {
    pub url: String,
    /// Which kind of location this is. The index always says, and every
    /// artifact has a `primary` location; a minimal client can download
    /// that one and ignore the rest.
    pub kind: Option<LocationKind>,
    /// Whether the vendor answers `url` with a redirect to a different
    /// host: a mirror network, or a CDN such as GitHub's asset host.
    /// The host itself is not recorded because mirror redirectors pick
    /// a different one per request.
    pub redirects_off_host: Option<bool>,
}

open_enum! {
    /// Which kind of download location this is.
    pub enum LocationKind {
        /// The vendor's main download tree.
        Primary,
        /// A vendor archive of builds no longer in the main tree.
        Vault,
        /// A mirror the vendor names.
        Mirror,
    }
}

/// Everything the vendor offers for checking a download. Clients choose
/// which of it to use, and whether to re-fetch it from the vendor
/// rather than trust the copy here.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Integrity {
    /// Hashes as read from the vendor. Empty when the vendor publishes
    /// none.
    #[serde(default)]
    pub digests: Vec<Digest>,
    /// Vendor signatures over the checksum documents or the image.
    #[serde(default)]
    pub signatures: Vec<Signature>,
    /// HTTP metadata the vendor's server reported for the file. Useful
    /// mostly when there are no digests. Its fields are null when the
    /// file redirects off-host (see the location's
    /// `redirects_off_host`), since each mirror reports its own.
    pub http: Option<HttpMetadata>,
}

/// A hash of the artifact as published by the vendor.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Digest {
    pub algorithm: DigestAlgorithm,
    /// Lowercase hex.
    pub value: String,
    /// Where the vendor publishes this hash, so a client can re-check it.
    pub source: Option<ChecksumSource>,
}

open_enum! {
    /// Hash algorithm of a digest.
    pub enum DigestAlgorithm {
        Sha256,
        Sha512,
    }
}

/// A vendor document that contains the artifact's hash.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ChecksumSource {
    pub url: String,
    pub format: ChecksumFormat,
    /// The name the file is listed under in the checksum document, for
    /// formats that list several files (`gnu`, `bsd`). Null for `bare`
    /// and `vendor_document`.
    pub filename: Option<String>,
}

open_enum! {
    /// Layout of a vendor document that contains a hash.
    pub enum ChecksumFormat {
        /// GNU coreutils style: `<hex>  [*]<filename>` per line.
        Gnu,
        /// BSD style: `SHA256 (<filename>) = <hex>` per line.
        Bsd,
        /// The hash is the file's first whitespace-separated token;
        /// anything after it is not a filename to match.
        Bare,
        /// Hash appears in a vendor document with no generic format (for
        /// example a JSON feed or an HTML page); re-checking it needs
        /// vendor-specific parsing.
        VendorDocument,
    }
}

/// A vendor OpenPGP signature over a checksum document or over the
/// image itself (see `signs`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Signature {
    /// URL of the signature itself.
    pub url: String,
    pub kind: SignatureKind,
    /// URL of what the signature covers: a checksum document, or the
    /// image itself (e.g. Alpine).
    pub signs: String,
    /// Where the vendor publishes the signing key, when known.
    pub key_url: Option<String>,
}

open_enum! {
    /// Kind of vendor signature.
    pub enum SignatureKind {
        /// Detached OpenPGP signature.
        PgpDetached,
        /// OpenPGP clear-signed document; `url` and `signs` are the same.
        PgpClearsigned,
    }
}

/// Response metadata the vendor's server reported for the file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct HttpMetadata {
    /// `Last-Modified`, verbatim.
    pub last_modified: Option<String>,
    /// `ETag`, verbatim. Opaque: not a content hash.
    pub etag: Option<String>,
}

// ---------------------------------------------------------------------
// The file tree as an API
// ---------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
pub struct DistroPath {
    pub distro: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ReleasePath {
    pub distro: String,
    pub release: String,
}

/// The landing page published as `index.html` at the root of the tree,
/// for people who open the index's address in a browser. It links to
/// the files by relative paths, so it works at any base URL.
pub const INDEX_HTML: &str = include_str!("index.html");

/// The API documentation page published as `docs/index.html`: Swagger UI
/// rendering `v1/openapi.json`.
pub const DOCS_HTML: &str = include_str!("docs.html");

/// The OpenAPI document describing the file tree. The index publishes
/// it as `v1/openapi.json`, and the tree checks require the published
/// copy to equal this.
pub fn openapi() -> Result<serde_json::Value, String> {
    let api = cloud_image_index_api_mod::stub_api_description()
        .map_err(|e| format!("API description rejected: {e}"))?;
    api.openapi("cloud-image-index", semver::Version::new(1, 0, 0))
        .json()
        .map_err(|e| format!("OpenAPI generation failed: {e}"))
}

/// The cloud-image-index file tree, version 1.
#[dropshot::api_description]
pub trait CloudImageIndexApi {
    type Context: Send + Sync + 'static;

    /// A landing page for people, linking to the files below.
    ///
    /// Unpublished: it is part of the tree but not of the format.
    #[endpoint { method = GET, path = "/index.html", unpublished = true }]
    async fn index_html(
        rqctx: RequestContext<Self::Context>,
    ) -> Result<http::Response<Body>, HttpError>;

    /// Browsable documentation of this API, rendered from the OpenAPI
    /// document.
    ///
    /// Unpublished: it is part of the tree but not of the format.
    #[endpoint { method = GET, path = "/docs/index.html", unpublished = true }]
    async fn docs_html(
        rqctx: RequestContext<Self::Context>,
    ) -> Result<http::Response<Body>, HttpError>;

    /// This OpenAPI document.
    ///
    /// As generated by the version of the index that wrote the tree.
    #[endpoint { method = GET, path = "/v1/openapi.json" }]
    async fn openapi(
        rqctx: RequestContext<Self::Context>,
    ) -> Result<HttpResponseOk<serde_json::Value>, HttpError>;

    /// Every distro in the index.
    #[endpoint { method = GET, path = "/v1/index.json" }]
    async fn distro_list(
        rqctx: RequestContext<Self::Context>,
    ) -> Result<HttpResponseOk<DistroList>, HttpError>;

    /// Every current build in the index, with its distro and release.
    #[endpoint { method = GET, path = "/v1/images.json" }]
    async fn images(
        rqctx: RequestContext<Self::Context>,
    ) -> Result<HttpResponseOk<ImageList>, HttpError>;

    /// One distro and its releases.
    ///
    /// Unpublished: part of the tree, which the generator reads back, but
    /// not of the format; `v1/images.json` has the same builds.
    #[endpoint { method = GET, path = "/v1/distros/{distro}/index.json", unpublished = true }]
    async fn distro(
        rqctx: RequestContext<Self::Context>,
        path: Path<DistroPath>,
    ) -> Result<HttpResponseOk<Distro>, HttpError>;

    /// The builds of the release holding the `latest` alias.
    ///
    /// Unpublished: part of the tree, which the generator reads back, but
    /// not of the format; `v1/images.json` has the same builds.
    #[endpoint { method = GET, path = "/v1/distros/{distro}/aliases/latest.json", unpublished = true }]
    async fn alias_latest(
        rqctx: RequestContext<Self::Context>,
        path: Path<DistroPath>,
    ) -> Result<HttpResponseOk<BuildList>, HttpError>;

    /// The builds of the release holding the `lts` alias.
    ///
    /// Not found for distros whose vendor has no LTS label.
    ///
    /// Unpublished: part of the tree, which the generator reads back, but
    /// not of the format; `v1/images.json` has the same builds.
    #[endpoint { method = GET, path = "/v1/distros/{distro}/aliases/lts.json", unpublished = true }]
    async fn alias_lts(
        rqctx: RequestContext<Self::Context>,
        path: Path<DistroPath>,
    ) -> Result<HttpResponseOk<BuildList>, HttpError>;

    /// The builds of the release holding the `dev` alias.
    ///
    /// Not found for distros with no development channel in the index.
    ///
    /// Unpublished: part of the tree, which the generator reads back, but
    /// not of the format; `v1/images.json` has the same builds.
    #[endpoint { method = GET, path = "/v1/distros/{distro}/aliases/dev.json", unpublished = true }]
    async fn alias_dev(
        rqctx: RequestContext<Self::Context>,
        path: Path<DistroPath>,
    ) -> Result<HttpResponseOk<BuildList>, HttpError>;

    /// Current builds of a release, from the vendor's main tree.
    ///
    /// Unpublished: part of the tree, which the generator reads back, but
    /// not of the format; `v1/images.json` has the same builds.
    #[endpoint { method = GET, path = "/v1/distros/{distro}/releases/{release}/index.json", unpublished = true }]
    async fn release_builds(
        rqctx: RequestContext<Self::Context>,
        path: Path<ReleasePath>,
    ) -> Result<HttpResponseOk<BuildList>, HttpError>;
}
