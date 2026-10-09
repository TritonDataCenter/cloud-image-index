// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Turning an index entry into an image build: which artifact, how to
//! decode it, how to check it, and how to name it.
//!
//! Checking is trust-but-verify: the downloaded image must match the
//! digest the index publishes *and* the same digest in the vendor's own
//! checksum file, fetched separately over HTTPS before the download.
//! This catches an index whose digest is wrong or stale, and an image
//! that changed at the vendor without its checksum file changing.
//!
//! It does not defend against a malicious index: the index says where
//! both the image and the checksum file are, so a tampered index can
//! point both at a host it controls. That needs trust anchored outside
//! the index, such as checking the vendors' signatures with keys the
//! client carries.
//!
//! An import the vendor cannot confirm needs `--allow-unverified`: a
//! vendor that publishes no digest (Talos), or one whose checksums live
//! only in a document with no generic layout (Oracle's HTML table). The
//! latter is still checked against the index's digest.

use anyhow::{Context, Result};
use client::types::{
    Artifact, Build, BuildList, ChecksumFormat, ChecksumFormatVariant0, Compression,
    CompressionVariant0, DigestAlgorithm, Distro, Image, ImageFormat, ImageFormatVariant0,
    ImageList, LocationKind, OsFamily, Release,
};
use nocloud_import::{ExpectedDigest, ImageInfo, SourceFormat};
use resolvers::verify::SumsStyle;
use url::Url;

/// What a release argument names: an alias some release of the distro
/// holds, or a release the distro lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requested<'a> {
    Alias(&'a str),
    Release(&'a str),
}

/// Check `name` against the releases and aliases `distro` lists, so a
/// wrong name gets a plain error naming the choices (the index's file
/// host answers a missing file with an HTML page, not a JSON error).
pub fn release_request<'a>(distro: &Distro, name: &'a str) -> Result<Requested<'a>> {
    let aliases: Vec<String> = distro.releases.iter().flat_map(alias_names).collect();
    if aliases.iter().any(|a| a == name) {
        return Ok(Requested::Alias(name));
    }
    if distro.releases.iter().any(|r| r.id == name) {
        return Ok(Requested::Release(name));
    }
    let releases: Vec<&str> = distro.releases.iter().map(|r| r.id.as_str()).collect();
    anyhow::bail!(
        "{} has no release or alias {name:?}; releases: {}; aliases: {}",
        distro.id,
        releases.join(", "),
        aliases.join(", ")
    )
}

/// The aliases `release` holds, spelled as the index spells them.
fn alias_names(release: &Release) -> Vec<String> {
    release
        .aliases
        .iter()
        .filter_map(|a| serde_json::to_value(a).ok())
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

/// The releases a user can import, one per line, each with its title and
/// the aliases it holds, for when no release was named.
pub fn release_choices(distro: &Distro) -> String {
    let width = distro
        .releases
        .iter()
        .map(|r| r.id.len())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for release in &distro.releases {
        let mut line = format!("  {:width$}", release.id);
        if let Some(title) = &release.title {
            line.push_str(&format!("  {title}"));
        }
        let aliases = alias_names(release);
        if !aliases.is_empty() {
            line.push_str(&format!("  [{}]", aliases.join(", ")));
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// The build to import (the first the index lists for the release) and
/// its artifact for `arch`: the one named `variant`, or the vendor's
/// default flavour.
pub fn choose_artifact<'a>(
    list: &'a BuildList,
    arch: &str,
    variant: Option<&str>,
) -> Result<(&'a Build, &'a Artifact)> {
    let build = list.builds.first().with_context(|| {
        format!(
            "{}/{}: the index lists no builds",
            list.distro, list.release
        )
    })?;
    let artifact = build
        .artifacts
        .iter()
        .filter(|a| a.arch == arch)
        .find(|a| match variant {
            Some(v) => a.variant.as_deref() == Some(v),
            None => a.default_variant == Some(true),
        })
        .with_context(|| {
            let have: Vec<String> = build
                .artifacts
                .iter()
                .map(|a| format!("{}/{}", a.variant.as_deref().unwrap_or("-"), a.arch))
                .collect();
            format!(
                "{}/{} build {}: no {} artifact for {arch} (have: {})",
                list.distro,
                list.release,
                build.build,
                variant.unwrap_or("default"),
                have.join(", ")
            )
        })?;
    Ok((build, artifact))
}

/// How the pipeline decodes an artifact of this format and compression.
pub fn source_format(format: &ImageFormat, compression: &Compression) -> Result<SourceFormat> {
    use CompressionVariant0 as C;
    use ImageFormatVariant0 as F;
    match (format, compression) {
        (ImageFormat::Variant0(F::Qcow2), Compression::Variant0(C::None)) => {
            Ok(SourceFormat::Qcow2)
        }
        (ImageFormat::Variant0(F::Raw), Compression::Variant0(C::None)) => Ok(SourceFormat::Raw),
        (ImageFormat::Variant0(F::Raw), Compression::Variant0(C::Xz)) => Ok(SourceFormat::Xz),
        (ImageFormat::Variant0(F::Raw), Compression::Variant0(C::Gzip)) => Ok(SourceFormat::RawGz),
        (ImageFormat::Variant0(F::Vmdk), Compression::Variant0(C::None)) => Ok(SourceFormat::Vmdk),
        _ => anyhow::bail!(
            "cannot import a {} image compressed with {}",
            serde_json::to_string(format)?,
            serde_json::to_string(compression)?
        ),
    }
}

/// Where to download the artifact: its primary location.
pub fn primary_url(artifact: &Artifact) -> Result<Url> {
    let location = artifact
        .locations
        .iter()
        .find(|l| l.kind == Some(LocationKind::Primary))
        .context("the artifact has no primary location")?;
    https_url(&location.url).with_context(|| format!("location {:?}", location.url))
}

/// Parse a URL the index gives, which must be HTTPS: over plain HTTP
/// anyone on the path could swap the image and the checksum file alike.
fn https_url(url: &str) -> Result<Url> {
    let url = Url::parse(url)?;
    anyhow::ensure!(url.scheme() == "https", "not an https URL");
    Ok(url)
}

/// A vendor checksum document with a generic layout, and the name the
/// artifact is listed under in it.
#[derive(Debug, Clone, PartialEq)]
pub struct VendorDocument {
    pub url: Url,
    pub filename: String,
    pub style: SumsStyle,
}

/// A digest the index publishes for the artifact, and the vendor document
/// that should agree with it (`None` when the vendor's document has no
/// generic layout to read).
#[derive(Debug, Clone)]
pub struct IndexDigest {
    pub algorithm: nocloud_import::DigestAlgorithm,
    pub hex: String,
    pub vendor: Option<VendorDocument>,
}

/// The digests the index publishes for `artifact`. Empty when the vendor
/// publishes none (e.g. Talos).
pub fn index_digests(artifact: &Artifact) -> Result<Vec<IndexDigest>> {
    let Some(integrity) = &artifact.integrity else {
        return Ok(Vec::new());
    };
    integrity
        .digests
        .iter()
        .map(|digest| {
            let algorithm = match digest.algorithm {
                DigestAlgorithm::Sha256 => nocloud_import::DigestAlgorithm::Sha256,
                DigestAlgorithm::Sha512 => nocloud_import::DigestAlgorithm::Sha512,
            };
            let vendor = match &digest.source {
                None => None,
                Some(source) => {
                    let style = match &source.format {
                        ChecksumFormat::Variant0(ChecksumFormatVariant0::Gnu) => {
                            Some(SumsStyle::Gnu)
                        }
                        ChecksumFormat::Variant0(ChecksumFormatVariant0::Bsd) => {
                            Some(SumsStyle::Bsd)
                        }
                        ChecksumFormat::Variant0(ChecksumFormatVariant0::Bare) => {
                            Some(SumsStyle::Bare)
                        }
                        _ => None,
                    };
                    match style {
                        None => None,
                        Some(style) => Some(VendorDocument {
                            url: https_url(&source.url)
                                .with_context(|| format!("checksum source {:?}", source.url))?,
                            filename: source.filename.clone().unwrap_or_default(),
                            style,
                        }),
                    }
                }
            };
            Ok(IndexDigest {
                algorithm,
                hex: digest.value.clone(),
                vendor,
            })
        })
        .collect()
}

/// Whether importing needs `--allow-unverified`: no digest has a vendor
/// checksum file the client can read to confirm it, either because the
/// vendor publishes no digest or because it publishes it only in a
/// document with no generic layout.
pub fn needs_allow_unverified(digests: &[IndexDigest]) -> bool {
    digests.iter().all(|d| d.vendor.is_none())
}

/// For each index digest, fetch the vendor's document and require it to
/// give the same digest, before anything is downloaded. Returns every
/// digest the image must then match, labelled by source, and notes on
/// digests only the index vouches for.
pub async fn confirm_with_vendor(
    http: &reqwest::Client,
    digests: &[IndexDigest],
) -> Result<(Vec<ExpectedDigest>, Vec<String>)> {
    let mut expected = Vec::new();
    let mut notes = Vec::new();
    for digest in digests {
        expected.push(ExpectedDigest {
            algorithm: digest.algorithm,
            hex: digest.hex.clone(),
            from: "the index".to_string(),
        });
        let Some(vendor) = &digest.vendor else {
            notes.push(format!(
                "the vendor publishes this {:?} only in a document with no generic \
                 layout; checking it against the index's digest alone",
                digest.algorithm
            ));
            continue;
        };
        let hex = resolvers::verify::fetch_expected_hash(
            http,
            &vendor.url,
            &vendor.filename,
            vendor.style,
        )
        .await
        .with_context(|| format!("read the vendor's checksum from {}", vendor.url))?;
        anyhow::ensure!(
            hex.eq_ignore_ascii_case(&digest.hex),
            "the index and the vendor disagree: the index says {} but {} says {hex}; \
             not importing",
            digest.hex,
            vendor.url
        );
        expected.push(ExpectedDigest {
            algorithm: digest.algorithm,
            hex,
            from: vendor.url.to_string(),
        });
    }
    Ok((expected, notes))
}

/// How the built image is named and described, following tritonadm's
/// nocloud images (`<distro>-<release>-nocloud`, version = the build).
/// A variant other than the vendor's default is appended to the version
/// (`9.8-20260525.0-lvm`), so two variants of a build differ in imgadm
/// and in the files the pipeline writes.
pub fn image_info(distro: &Distro, release: &str, build: &Build, artifact: &Artifact) -> ImageInfo {
    let os = match distro.os_family {
        OsFamily::Linux => "linux",
        OsFamily::Bsd => "bsd",
        OsFamily::Illumos => "illumos",
    };
    let name = distro.name.as_deref().unwrap_or(&distro.id);
    let title = distro
        .releases
        .iter()
        .find(|r| r.id == release)
        .and_then(|r| r.title.as_deref())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{name} {release}"));
    let version = match (&artifact.variant, artifact.default_variant) {
        (Some(variant), Some(false) | None) => format!("{}-{variant}", build.build),
        _ => build.build.clone(),
    };
    ImageInfo {
        vendor: distro.id.clone(),
        series: release.to_string(),
        version,
        os: os.to_string(),
        description: format!(
            "{title} CloudInit NoCloud compatible image. Built to run on bhyve virtual machines."
        ),
        homepage: distro.homepage.clone().unwrap_or_default(),
        ssh_key: artifact.ssh_key_injection.unwrap_or(true),
    }
}

/// The name of the directories an import keeps its download and its
/// output in: one per build and variant, since the pipeline reuses a
/// download it finds there, and some vendors name every build's file
/// alike.
pub fn directory_name(info: &ImageInfo) -> String {
    format!("{}-{}-{}", info.vendor, info.series, info.version)
}

/// How an import would be checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    /// The vendor's checksum file confirms the index's digest.
    Vendor,
    /// Only the index's digest is checked (`--allow-unverified`).
    Index,
    /// Nothing but TLS (`--allow-unverified`).
    None,
}

impl Check {
    pub fn of(digests: &[IndexDigest]) -> Check {
        if digests.is_empty() {
            Check::None
        } else if needs_allow_unverified(digests) {
            Check::Index
        } else {
            Check::Vendor
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Check::Vendor => "vendor",
            Check::Index => "index",
            Check::None => "none",
        }
    }
}

/// The manifest UUID the import will give the image, known before the
/// download when the index publishes a sha256.
pub fn index_uuid(digests: &[IndexDigest]) -> Option<uuid::Uuid> {
    digests
        .iter()
        .find(|d| d.algorithm == nocloud_import::DigestAlgorithm::Sha256)
        .map(|d| nocloud_import::stable_manifest_uuid(&d.hex.to_lowercase()))
}

/// A release's current image, as `avail` lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct Available {
    pub distro: String,
    pub release: String,
    pub name: String,
    pub version: String,
    pub os: String,
    pub aliases: Vec<String>,
    pub check: Check,
    pub uuid: Option<uuid::Uuid>,
}

/// The image an import of the release in `builds` would produce, with the
/// vendor's default variant.
pub fn available(distro: &Distro, builds: &BuildList) -> Result<Available> {
    let (build, artifact) = choose_artifact(builds, "x86_64", None)?;
    let info = image_info(distro, &builds.release, build, artifact);
    let digests = index_digests(artifact)?;
    let aliases = distro
        .releases
        .iter()
        .find(|r| r.id == builds.release)
        .map(alias_names)
        .unwrap_or_default();
    Ok(Available {
        distro: distro.id.clone(),
        release: builds.release.clone(),
        name: info.image_name(),
        version: info.version,
        os: info.os,
        aliases,
        check: Check::of(&digests),
        uuid: index_uuid(&digests),
    })
}

/// The images `avail` lists, from the index's `v1/images.json`: each
/// release's newest build (the one an import of the release takes), of
/// every distro or only `only`, in the list's order. Also returns notes on
/// releases this client cannot build, which are left out.
pub fn avail_rows(list: &ImageList, only: Option<&str>) -> Result<(Vec<Available>, Vec<String>)> {
    if let Some(id) = only
        && !list.images.iter().any(|i| i.distro.id == id)
    {
        let mut ids: Vec<&str> = Vec::new();
        for image in &list.images {
            if !ids.contains(&image.distro.id.as_str()) {
                ids.push(&image.distro.id);
            }
        }
        anyhow::bail!("the index has no distro {id:?}; it has: {}", ids.join(", "));
    }
    let mut rows = Vec::new();
    let mut notes = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for image in &list.images {
        if only.is_some_and(|id| image.distro.id != id) {
            continue;
        }
        // Builds are newest first; later ones of a release are older.
        if !seen.insert((&image.distro.id, &image.release.id)) {
            continue;
        }
        let (distro, builds) = one_image(image);
        match available(&distro, &builds) {
            Ok(row) => rows.push(row),
            Err(e) => notes.push(format!(
                "leaving out {} {}: {e:#}",
                image.distro.id, image.release.id
            )),
        }
    }
    Ok((rows, notes))
}

/// An entry of `v1/images.json` as the distro and build list the rest of
/// this module reads: the distro with only this release, and the release
/// with only this build. The entry has no `dev_channel`, which nothing
/// here uses.
fn one_image(image: &Image) -> (Distro, BuildList) {
    let distro = Distro {
        id: image.distro.id.clone(),
        name: image.distro.name.clone(),
        os_family: image.distro.os_family,
        homepage: image.distro.homepage.clone(),
        dev_channel: None,
        releases: vec![image.release.clone()],
    };
    let builds = BuildList {
        distro: image.distro.id.clone(),
        release: image.release.id.clone(),
        builds: vec![image.build.clone()],
    };
    (distro, builds)
}

/// The image with this UUID, for `import <uuid>`.
pub fn find_by_uuid(rows: &[Available], uuid: uuid::Uuid) -> Option<&Available> {
    rows.iter().find(|r| r.uuid == Some(uuid))
}

/// `rows` as a table in the style of `imgadm avail`.
pub fn avail_table(rows: &[Available], header: bool) -> String {
    let mut lines: Vec<[String; 6]> = Vec::new();
    if header {
        lines.push(["UUID", "NAME", "VERSION", "OS", "ALIASES", "CHECK"].map(str::to_string));
    }
    for row in rows {
        lines.push([
            row.uuid.map_or("-".to_string(), |u| u.to_string()),
            row.name.clone(),
            row.version.clone(),
            row.os.clone(),
            row.aliases.join(","),
            row.check.as_str().to_string(),
        ]);
    }
    let mut widths = [0; 6];
    for line in &lines {
        for (width, cell) in widths.iter_mut().zip(line) {
            *width = (*width).max(cell.len());
        }
    }
    let mut out = String::new();
    for line in &lines {
        let cells: Vec<String> = line
            .iter()
            .zip(widths)
            .map(|(cell, width)| format!("{cell:width$}"))
            .collect();
        out.push_str(cells.join("  ").trim_end());
        out.push('\n');
    }
    out
}

/// `rows` as JSON, for scripts (`avail -j`).
pub fn avail_json(rows: &[Available]) -> serde_json::Value {
    rows.iter()
        .map(|r| {
            serde_json::json!({
                "uuid": r.uuid.map(|u| u.to_string()),
                "name": r.name,
                "version": r.version,
                "os": r.os,
                "distro": r.distro,
                "release": r.release,
                "aliases": r.aliases,
                "check": r.check.as_str(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use client::types::{BuildList, Distro};

    fn list(json: &str) -> BuildList {
        serde_json::from_str(json).unwrap_or_else(|e| panic!("{e}"))
    }
    fn ubuntu() -> BuildList {
        list(include_str!(
            "../../../examples/v1/distros/ubuntu/aliases/latest.json"
        ))
    }
    fn rocky9() -> BuildList {
        list(include_str!(
            "../../../examples/v1/distros/rocky/releases/9/index.json"
        ))
    }
    fn talos() -> BuildList {
        list(include_str!(
            "../../../examples/v1/distros/talos/aliases/latest.json"
        ))
    }
    fn ubuntu_distro() -> Distro {
        serde_json::from_str(include_str!(
            "../../../examples/v1/distros/ubuntu/index.json"
        ))
        .unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn the_default_variant_is_chosen_unless_one_is_named() -> anyhow::Result<()> {
        let rocky = rocky9();
        let (build, artifact) = choose_artifact(&rocky, "x86_64", None)?;
        assert_eq!(build.build, "9.8-20260525.0");
        assert_eq!(artifact.variant.as_deref(), Some("base"));
        let (_, lvm) = choose_artifact(&rocky, "x86_64", Some("lvm"))?;
        assert_eq!(lvm.variant.as_deref(), Some("lvm"));
        assert!(choose_artifact(&rocky, "aarch64", None).is_err());
        assert!(choose_artifact(&rocky, "x86_64", Some("nope")).is_err());
        Ok(())
    }

    #[test]
    fn formats_map_onto_what_the_pipeline_decodes() -> anyhow::Result<()> {
        let ubuntu = ubuntu();
        let (_, ubuntu_artifact) = choose_artifact(&ubuntu, "x86_64", None)?;
        assert!(matches!(
            source_format(&ubuntu_artifact.format, &ubuntu_artifact.compression)?,
            nocloud_import::SourceFormat::Qcow2
        ));
        let talos = talos();
        let (_, talos_artifact) = choose_artifact(&talos, "x86_64", None)?;
        assert!(matches!(
            source_format(&talos_artifact.format, &talos_artifact.compression)?,
            nocloud_import::SourceFormat::Xz
        ));
        let zstd: client::types::Compression = serde_json::from_str("\"zstd\"")?;
        assert!(source_format(&talos_artifact.format, &zstd).is_err());
        Ok(())
    }

    #[test]
    fn each_index_digest_names_the_vendor_document_that_should_agree() -> anyhow::Result<()> {
        let ubuntu = ubuntu();
        let (_, artifact) = choose_artifact(&ubuntu, "x86_64", None)?;
        let digests = index_digests(artifact)?;
        assert_eq!(digests.len(), 1);
        let vendor = digests[0]
            .vendor
            .as_ref()
            .unwrap_or_else(|| panic!("a gnu document is readable"));
        assert!(vendor.url.as_str().ends_with("/SHA256SUMS"));
        assert_eq!(vendor.filename, "ubuntu-26.04-server-cloudimg-amd64.img");
        assert_eq!(vendor.style, resolvers::verify::SumsStyle::Gnu);
        Ok(())
    }

    #[test]
    fn an_artifact_without_a_digest_has_nothing_to_check() -> anyhow::Result<()> {
        let talos = talos();
        let (_, artifact) = choose_artifact(&talos, "x86_64", None)?;
        assert!(index_digests(artifact)?.is_empty());
        Ok(())
    }

    #[test]
    fn the_image_is_named_like_tritonadm_names_it() -> anyhow::Result<()> {
        let ubuntu = ubuntu();
        let (build, artifact) = choose_artifact(&ubuntu, "x86_64", None)?;
        let info = image_info(&ubuntu_distro(), &ubuntu.release, build, artifact);
        assert_eq!(info.vendor, "ubuntu");
        assert_eq!(info.series, "resolute");
        assert_eq!(info.version, "20260927");
        assert_eq!(info.os, "linux");
        assert!(info.ssh_key);
        assert_eq!(info.homepage, "https://ubuntu.com/");
        Ok(())
    }

    fn rocky_distro() -> Distro {
        serde_json::from_str(include_str!(
            "../../../examples/v1/distros/rocky/index.json"
        ))
        .unwrap_or_else(|e| panic!("{e}"))
    }

    /// Two variants of one build must not share a name in imgadm, nor
    /// overwrite each other's files: a variant other than the default is
    /// part of the version.
    #[test]
    fn a_variant_other_than_the_default_is_part_of_the_version() -> anyhow::Result<()> {
        let rocky = rocky9();
        let (build, base) = choose_artifact(&rocky, "x86_64", None)?;
        let base = image_info(&rocky_distro(), &rocky.release, build, base);
        assert_eq!(base.version, "9.8-20260525.0");
        let (build, lvm) = choose_artifact(&rocky, "x86_64", Some("lvm"))?;
        let lvm = image_info(&rocky_distro(), &rocky.release, build, lvm);
        assert_eq!(lvm.version, "9.8-20260525.0-lvm");
        Ok(())
    }

    /// The download cache and the output are kept per build and variant:
    /// a vendor that names every build's file alike (Talos) must not have
    /// an earlier build's download reused for a later one.
    #[test]
    fn each_build_and_variant_gets_its_own_directory() -> anyhow::Result<()> {
        let rocky = rocky9();
        let (build, lvm) = choose_artifact(&rocky, "x86_64", Some("lvm"))?;
        let info = image_info(&rocky_distro(), &rocky.release, build, lvm);
        assert_eq!(directory_name(&info), "rocky-9-9.8-20260525.0-lvm");
        let talos = talos();
        let (build, artifact) = choose_artifact(&talos, "x86_64", None)?;
        let mut later = build.clone();
        later.build = format!("{}-later", build.build);
        let distro = Distro {
            id: "talos".to_string(),
            ..ubuntu_distro()
        };
        assert_ne!(
            directory_name(&image_info(&distro, &talos.release, build, artifact)),
            directory_name(&image_info(&distro, &talos.release, &later, artifact)),
        );
        Ok(())
    }

    fn ubuntu_row() -> Available {
        available(&ubuntu_distro(), &ubuntu()).unwrap_or_else(|e| panic!("{e:#}"))
    }

    /// `avail` shows an image as `imgadm list` will after the import:
    /// the same name, version and UUID.
    #[test]
    fn an_available_image_is_shown_as_imgadm_will_show_it() -> anyhow::Result<()> {
        let row = ubuntu_row();
        assert_eq!(row.distro, "ubuntu");
        assert_eq!(row.release, "resolute");
        assert_eq!(row.name, "ubuntu-resolute-nocloud");
        assert_eq!(row.version, "20260927");
        assert_eq!(row.os, "linux");
        assert_eq!(row.aliases, ["latest", "lts"]);
        assert_eq!(row.check, Check::Vendor);
        let ubuntu = ubuntu();
        let (_, artifact) = choose_artifact(&ubuntu, "x86_64", None)?;
        let sha256 = index_digests(artifact)?
            .into_iter()
            .find(|d| d.algorithm == nocloud_import::DigestAlgorithm::Sha256)
            .map(|d| d.hex)
            .unwrap_or_default();
        assert_eq!(
            row.uuid,
            Some(nocloud_import::stable_manifest_uuid(&sha256.to_lowercase()))
        );
        Ok(())
    }

    fn images() -> client::types::ImageList {
        serde_json::from_str(include_str!("../../../examples/v1/images.json"))
            .unwrap_or_else(|e| panic!("{e}"))
    }

    /// `avail` reads v1/images.json and shows the same rows it showed
    /// from the distro and release files.
    #[test]
    fn avail_rows_come_from_the_image_list() -> anyhow::Result<()> {
        let (rows, notes) = avail_rows(&images(), None)?;
        assert!(notes.is_empty(), "{notes:?}");
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "ubuntu-resolute-nocloud",
                "ubuntu-noble-nocloud",
                "ubuntu-jammy-nocloud",
                "rocky-10-nocloud",
                "rocky-9-nocloud",
                "rocky-8-nocloud",
                "talos-1.14-nocloud",
            ]
        );
        assert_eq!(rows[0], ubuntu_row());
        let (rocky, _) = avail_rows(&images(), Some("rocky"))?;
        assert_eq!(rocky.len(), 3);
        Ok(())
    }

    /// Only a release's newest build is listed: it is the one an import
    /// of the release takes.
    #[test]
    fn avail_lists_each_releases_newest_build() -> anyhow::Result<()> {
        let mut list = images();
        let mut older = list.images[0].clone();
        older.build.build = "20200101".to_string();
        list.images.insert(1, older);
        let (rows, _) = avail_rows(&list, Some("ubuntu"))?;
        let versions: Vec<&str> = rows.iter().map(|r| r.version.as_str()).collect();
        assert_eq!(versions, ["20260927", "20260926", "20261004"]);
        Ok(())
    }

    #[test]
    fn avail_names_the_distros_when_one_is_unknown() {
        let err = avail_rows(&images(), Some("nosuch"))
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(
            err.contains("no distro \"nosuch\"") && err.contains("ubuntu, rocky, talos"),
            "{err}"
        );
    }

    /// How each image would be checked, and so whether it needs
    /// `--allow-unverified`; no sha256, no UUID before the download.
    #[test]
    fn an_available_image_says_how_it_would_be_checked() {
        let distro = Distro {
            id: "talos".to_string(),
            ..ubuntu_distro()
        };
        let talos = talos();
        let row = available(&distro, &talos).unwrap_or_else(|e| panic!("{e:#}"));
        assert_eq!(row.check, Check::None);
        assert_eq!(row.uuid, None);
        let mut ubuntu = ubuntu();
        ubuntu.builds[0].artifacts[0] = edited(&ubuntu, |a| {
            a["integrity"]["digests"][0]["source"]["format"] = "vendor_document".into()
        });
        let row = available(&ubuntu_distro(), &ubuntu).unwrap_or_else(|e| panic!("{e:#}"));
        assert_eq!(row.check, Check::Index);
    }

    #[test]
    fn the_table_lines_up_like_imgadm_avail() {
        let ubuntu = ubuntu_row();
        let mut talos = ubuntu.clone();
        talos.name = "talos-1.14-nocloud".to_string();
        talos.version = "1.14.2".to_string();
        talos.aliases = vec!["latest".to_string()];
        talos.check = Check::None;
        talos.uuid = None;
        let uuid = ubuntu.uuid.map(|u| u.to_string()).unwrap_or_default();
        let rows = [ubuntu, talos];
        assert_eq!(
            avail_table(&rows, true),
            format!(
                "UUID                                  NAME                     VERSION   OS     ALIASES     CHECK\n\
                 {uuid}  ubuntu-resolute-nocloud  20260927  linux  latest,lts  vendor\n\
                 -                                     talos-1.14-nocloud       1.14.2    linux  latest      none\n"
            )
        );
        assert!(!avail_table(&rows, false).contains("UUID"));
    }

    #[test]
    fn a_uuid_finds_its_image() {
        let row = ubuntu_row();
        let rows = [row.clone()];
        let uuid = row.uuid.unwrap_or_else(|| panic!("ubuntu has a sha256"));
        assert_eq!(
            find_by_uuid(&rows, uuid).map(|r| &r.release),
            Some(&row.release)
        );
        assert!(find_by_uuid(&rows, uuid::Uuid::nil()).is_none());
    }

    /// Without a release, the user is shown what they could import.
    #[test]
    fn the_choices_list_each_release_with_its_title_and_aliases() {
        assert_eq!(
            release_choices(&ubuntu_distro()),
            "  resolute  26.04 LTS Resolute Raccoon  [latest, lts]\n\
             \x20 noble     24.04 LTS Noble Numbat\n\
             \x20 jammy     22.04 LTS Jammy Jellyfish\n"
        );
    }

    #[test]
    fn a_release_or_alias_must_be_one_the_distro_lists() {
        let ubuntu = ubuntu_distro();
        assert_eq!(
            release_request(&ubuntu, "latest").ok(),
            Some(Requested::Alias("latest"))
        );
        assert_eq!(
            release_request(&ubuntu, "lts").ok(),
            Some(Requested::Alias("lts"))
        );
        assert_eq!(
            release_request(&ubuntu, "noble").ok(),
            Some(Requested::Release("noble"))
        );
        let err = release_request(&ubuntu, "warty")
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(
            err.contains("noble") && err.contains("latest"),
            "lists the choices: {err}"
        );
        assert!(
            release_request(&ubuntu, "dev").is_err(),
            "no release holds dev"
        );
    }

    /// The first artifact of `list`, edited as JSON.
    fn edited(list: &BuildList, edit: impl Fn(&mut serde_json::Value)) -> Artifact {
        let mut json =
            serde_json::to_value(&list.builds[0].artifacts[0]).unwrap_or_else(|e| panic!("{e}"));
        edit(&mut json);
        serde_json::from_value(json).unwrap_or_else(|e| panic!("{e}"))
    }

    /// Plain HTTP would let anyone on the path swap the image and the
    /// vendor's checksum file alike.
    #[test]
    fn downloads_and_checksum_files_must_use_https() {
        let ubuntu = ubuntu();
        for url in ["http://cloud-images.ubuntu.com/x.img", "file:///etc/passwd"] {
            let artifact = edited(&ubuntu, |a| a["locations"][0]["url"] = url.into());
            assert!(primary_url(&artifact).is_err(), "{url}");
        }
        let artifact = edited(&ubuntu, |a| {
            let source = &mut a["integrity"]["digests"][0]["source"]["url"];
            *source = source
                .as_str()
                .unwrap_or_default()
                .replace("https:", "http:")
                .into();
        });
        assert!(index_digests(&artifact).is_err());
    }

    /// The vendor can confirm the index only through a checksum file the
    /// client can read; otherwise the import is unverified.
    #[test]
    fn only_a_generic_checksum_file_lets_the_vendor_confirm() -> anyhow::Result<()> {
        let ubuntu = ubuntu();
        let (_, artifact) = choose_artifact(&ubuntu, "x86_64", None)?;
        assert!(!needs_allow_unverified(&index_digests(artifact)?));
        for format in ["vendor_document", "some_future_format"] {
            let artifact = edited(&ubuntu, |a| {
                a["integrity"]["digests"][0]["source"]["format"] = format.into()
            });
            assert!(
                needs_allow_unverified(&index_digests(&artifact)?),
                "{format}"
            );
        }
        let talos = talos();
        let (_, artifact) = choose_artifact(&talos, "x86_64", None)?;
        assert!(needs_allow_unverified(&index_digests(artifact)?));
        Ok(())
    }

    #[test]
    fn the_download_url_is_the_primary_location() -> anyhow::Result<()> {
        let ubuntu = ubuntu();
        let (_, artifact) = choose_artifact(&ubuntu, "x86_64", None)?;
        assert!(
            primary_url(artifact)?
                .as_str()
                .ends_with("server-cloudimg-amd64.img")
        );
        Ok(())
    }
}
