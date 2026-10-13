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
    Artifact, Build, Compression, Image, ImageFormat, ImageList, OsFamily, Release,
};
use nocloud_import::{ExpectedDigest, ImageInfo, SourceFormat};
use resolvers::verify::SumsStyle;
use url::Url;

/// A distro, as much as an import needs: what `v1/images.json` gives for
/// it, its releases in order (see [`distro_from_images`]).
#[derive(Debug, Clone)]
#[cfg_attr(test, derive(serde::Deserialize))]
pub struct Distro {
    pub id: String,
    pub name: Option<String>,
    pub os_family: OsFamily,
    pub homepage: Option<String>,
    pub releases: Vec<Release>,
}

/// One release's builds, newest first.
#[derive(Debug, Clone)]
#[cfg_attr(test, derive(serde::Deserialize))]
pub struct BuildList {
    pub distro: String,
    pub release: String,
    pub builds: Vec<Build>,
}

/// What a release argument names: an alias some release of the distro
/// holds, or a release the distro lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requested<'a> {
    Alias(&'a str),
    Release(&'a str),
}

/// Check `name` against the releases and aliases `distro` lists, so a
/// wrong name gets a plain error naming the choices.
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
    release.aliases.iter().map(|a| a.to_string()).collect()
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
    // Open enums arrive as strings: a value this client does not know
    // falls through to the error.
    match (format.as_str(), compression.as_str()) {
        ("qcow2", "none") => Ok(SourceFormat::Qcow2),
        ("raw", "none") => Ok(SourceFormat::Raw),
        ("raw", "xz") => Ok(SourceFormat::Xz),
        ("raw", "gzip") => Ok(SourceFormat::RawGz),
        ("vmdk", "none") => Ok(SourceFormat::Vmdk),
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
        .find(|l| l.kind.as_ref().is_some_and(|k| k.as_str() == "primary"))
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

/// The digests the index publishes for `artifact` in an algorithm this
/// client can check. Empty when the vendor publishes none (e.g. Talos),
/// or only in algorithms added to the index after this client.
pub fn index_digests(artifact: &Artifact) -> Result<Vec<IndexDigest>> {
    let Some(integrity) = &artifact.integrity else {
        return Ok(Vec::new());
    };
    integrity
        .digests
        .iter()
        .filter_map(|digest| {
            let algorithm = match digest.algorithm.as_str() {
                "sha256" => nocloud_import::DigestAlgorithm::Sha256,
                "sha512" => nocloud_import::DigestAlgorithm::Sha512,
                _ => return None,
            };
            Some((digest, algorithm))
        })
        .map(|(digest, algorithm)| {
            let vendor = match &digest.source {
                None => None,
                Some(source) => {
                    let style = match source.format.as_str() {
                        "gnu" => Some(SumsStyle::Gnu),
                        "bsd" => Some(SumsStyle::Bsd),
                        "bare" => Some(SumsStyle::Bare),
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

/// Whether the image reads its configuration with cloud-init's NoCloud
/// datasource, which is how SmartOS gives a bhyve guest its cloud-init
/// configuration.
pub fn reads_nocloud(artifact: &Artifact) -> bool {
    artifact.datasources.iter().any(|d| d == "nocloud")
}

/// What to tell someone about to use an image that does not read NoCloud.
pub fn without_nocloud_note(distro: &str, release: &str) -> String {
    format!(
        "{distro} {release} does not use cloud-init's NoCloud datasource: a \
         guest made from it does not read the cloud-init configuration \
         (user-data, SSH keys) SmartOS gives through it"
    )
}

/// The algorithms of the artifact's digests that this client cannot
/// check, because they were added to the index after it.
pub fn unknown_digest_algorithms(artifact: &Artifact) -> Vec<String> {
    let Some(integrity) = &artifact.integrity else {
        return Vec::new();
    };
    integrity
        .digests
        .iter()
        .map(|d| d.algorithm.as_str())
        .filter(|a| !matches!(*a, "sha256" | "sha512"))
        .map(str::to_string)
        .collect()
}

/// What to tell someone about to use an image with a digest this client
/// cannot check.
pub fn unknown_digest_note(algorithms: &[String]) -> String {
    format!(
        "the index has a {} digest for it, which this cloud-image cannot \
         check; run `cloud-image self-update` to check it",
        algorithms.join(" and ")
    )
}

/// Why an import needs `--allow-unverified` (see
/// [`needs_allow_unverified`]): `vendor`'s digests the client can check,
/// and the algorithms of those it cannot.
pub fn why_unverified(vendor: &str, digests: &[IndexDigest], unknown: &[String]) -> String {
    if !unknown.is_empty() {
        return format!(
            "{}, or pass --allow-unverified to import it without that check",
            unknown_digest_note(unknown)
        );
    }
    if digests.is_empty() {
        return format!(
            "{vendor} publishes no digest for this image; pass --allow-unverified \
             to import it trusting TLS alone"
        );
    }
    format!(
        "{vendor} gives this image's digest only in a document with no generic \
         layout, so it cannot confirm the index's digest; pass \
         --allow-unverified to check the image against the index alone"
    )
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
    let nocloud = reads_nocloud(artifact);
    let description = if nocloud {
        format!(
            "{title} CloudInit NoCloud compatible image. Built to run on bhyve virtual machines."
        )
    } else {
        format!(
            "{title} image, without cloud-init's NoCloud datasource. Built to run on bhyve \
             virtual machines."
        )
    };
    ImageInfo {
        vendor: distro.id.clone(),
        series: release.to_string(),
        version,
        os: os.to_string(),
        description,
        homepage: distro.homepage.clone().unwrap_or_default(),
        // When the index does not say, a NoCloud image takes keys from
        // metadata, as cloud-init does by default.
        ssh_key: artifact.ssh_key_injection.unwrap_or(nocloud),
    }
}

/// The name of the directories an import keeps its download and its
/// output in: one per build and variant, since the pipeline reuses a
/// download it finds there, and some vendors name every build's file
/// alike. Its parts come from the index, so [`check_ids`] must accept
/// them first.
pub fn directory_name(info: &ImageInfo) -> String {
    format!("{}-{}-{}", info.vendor, info.series, info.version)
}

/// Require the ids an import names files and directories after to follow
/// the index's id rule, so none can climb out of the directory it is made
/// in. `--index` can point at any index, so the client checks them too.
pub fn check_ids(distro: &Distro, release: &str, build: &Build, artifact: &Artifact) -> Result<()> {
    let ids = [
        ("distro id", Some(distro.id.as_str())),
        ("release id", Some(release)),
        ("build id", Some(build.build.as_str())),
        ("variant", artifact.variant.as_deref()),
    ];
    for (what, id) in ids {
        if let Some(id) = id {
            resolvers::validate_version_token(id).with_context(|| what.to_string())?;
        }
    }
    Ok(())
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
    /// Whether the local image store has it; `None` when the store was
    /// not read (off SmartOS).
    pub installed: Option<bool>,
    /// Whether the image reads NoCloud (see [`reads_nocloud`]).
    pub nocloud: bool,
    /// Algorithms of its digests this client cannot check (see
    /// [`unknown_digest_algorithms`]).
    pub unknown_digests: Vec<String>,
    /// Whether `uuid` came from the local image store rather than the
    /// index (see [`mark_installed`]).
    pub uuid_from_store: bool,
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
        installed: None,
        nocloud: reads_nocloud(artifact),
        unknown_digests: unknown_digest_algorithms(artifact),
        uuid_from_store: false,
    })
}

/// The images `avail` lists, from the index's `v1/images.json`: each
/// release's newest build (the one an import of the release takes), of
/// every distro or only `only`, in the list's order. Also returns notes on
/// releases this client cannot build, which are left out.
pub fn avail_rows(list: &ImageList, only: Option<&str>) -> Result<(Vec<Available>, Vec<String>)> {
    if let Some(id) = only {
        known_distro(list, id)?;
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
            Ok(row) => {
                // Only for one distro: every plain `avail` would repeat it.
                if !row.nocloud && only.is_some() {
                    notes.push(without_nocloud_note(&row.distro, &row.release));
                }
                if !row.unknown_digests.is_empty() {
                    notes.push(format!(
                        "{} {}: {}",
                        row.distro,
                        row.release,
                        unknown_digest_note(&row.unknown_digests)
                    ));
                }
                rows.push(row);
            }
            Err(e) => notes.push(format!(
                "leaving out {} {}: {e:#}",
                image.distro.id, image.release.id
            )),
        }
    }
    Ok((rows, notes))
}

/// Fail unless the image list has a distro `id`, naming the ones it has.
fn known_distro(list: &ImageList, id: &str) -> Result<()> {
    if list.images.iter().any(|i| i.distro.id == id) {
        return Ok(());
    }
    let mut ids: Vec<&str> = Vec::new();
    for image in &list.images {
        if !ids.contains(&image.distro.id.as_str()) {
            ids.push(&image.distro.id);
        }
    }
    anyhow::bail!("the index has no distro {id:?}; it has: {}", ids.join(", "))
}

/// The distro `id` as `v1/images.json` gives it: the distro, with each
/// release that has a current build, in the list's order, and alongside,
/// each release's builds, newest first.
pub fn distro_from_images(list: &ImageList, id: &str) -> Result<(Distro, Vec<BuildList>)> {
    known_distro(list, id)?;
    let mut distro: Option<Distro> = None;
    let mut builds: Vec<BuildList> = Vec::new();
    for image in list.images.iter().filter(|i| i.distro.id == id) {
        let distro = distro.get_or_insert_with(|| Distro {
            id: image.distro.id.clone(),
            name: image.distro.name.clone(),
            os_family: image.distro.os_family,
            homepage: image.distro.homepage.clone(),
            releases: Vec::new(),
        });
        match builds.iter_mut().find(|b| b.release == image.release.id) {
            Some(list) => list.builds.push(image.build.clone()),
            None => {
                distro.releases.push(image.release.clone());
                builds.push(BuildList {
                    distro: image.distro.id.clone(),
                    release: image.release.id.clone(),
                    builds: vec![image.build.clone()],
                });
            }
        }
    }
    let distro = distro.with_context(|| format!("the index has no distro {id:?}"))?;
    Ok((distro, builds))
}

/// The builds of the release `request` names: the one holding the alias,
/// or the one of that name. `builds` is in the order of `distro.releases`,
/// as [`distro_from_images`] gives them.
pub fn requested_builds<'a>(
    distro: &Distro,
    builds: &'a [BuildList],
    request: Requested<'_>,
) -> Result<&'a BuildList> {
    let index = distro.releases.iter().position(|r| match request {
        Requested::Alias(alias) => alias_names(r).iter().any(|a| a == alias),
        Requested::Release(id) => r.id == id,
    });
    index
        .and_then(|i| builds.get(i))
        .with_context(|| format!("{}: no release for {request:?}", distro.id))
}

/// An entry of `v1/images.json` as the distro and build list the rest of
/// this module reads: the distro with only this release, and the release
/// with only this build.
fn one_image(image: &Image) -> (Distro, BuildList) {
    let distro = Distro {
        id: image.distro.id.clone(),
        name: image.distro.name.clone(),
        os_family: image.distro.os_family,
        homepage: image.distro.homepage.clone(),
        releases: vec![image.release.clone()],
    };
    let builds = BuildList {
        distro: image.distro.id.clone(),
        release: image.release.id.clone(),
        builds: vec![image.build.clone()],
    };
    (distro, builds)
}

/// For `import <uuid>`: the image about to be imported must be the one
/// asked for. The UUID names a row of `avail`, and the import then
/// resolves that row's release again; this holds the two to the same
/// build, and nothing is imported if they differ.
pub fn confirm_requested_uuid(
    requested: Option<uuid::Uuid>,
    resolved: Option<uuid::Uuid>,
) -> Result<()> {
    if let Some(requested) = requested {
        anyhow::ensure!(
            resolved == Some(requested),
            "asked for image {requested}, but the index now gives {}; see \
             `cloud-image avail`",
            resolved.map_or("an image with no known UUID".to_string(), |u| u.to_string())
        );
    }
    Ok(())
}

/// Why `import <uuid>` found no image with that UUID.
pub fn uuid_not_listed(uuid: uuid::Uuid) -> String {
    format!("no image `cloud-image avail` lists has UUID {uuid}")
}

/// The image with this UUID, for `import <uuid>`.
pub fn find_by_uuid(rows: &[Available], uuid: uuid::Uuid) -> Option<&Available> {
    rows.iter().find(|r| r.uuid == Some(uuid))
}

/// An image in the local image store, as `imgadm list -j` lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct Installed {
    pub uuid: Option<uuid::Uuid>,
    pub name: String,
    pub version: String,
}

/// The images in `imgadm list -j` output: each entry is an image's stored
/// record, `{manifest, zpool, ...}`.
pub fn parse_imgadm_list(json: &str) -> Result<Vec<Installed>> {
    let records: Vec<serde_json::Value> =
        serde_json::from_str(json).context("parse `imgadm list -j` output")?;
    Ok(records
        .iter()
        .filter_map(|r| r.get("manifest"))
        .map(|m| {
            let text = |key: &str| m.get(key).and_then(|v| v.as_str()).unwrap_or_default();
            Installed {
                uuid: uuid::Uuid::parse_str(text("uuid")).ok(),
                name: text("name").to_string(),
                version: text("version").to_string(),
            }
        })
        .collect())
}

/// The installed image an import would produce: one with the same UUID,
/// or, since an image whose vendor publishes no sha256 has no UUID before
/// its download, the same name and version.
pub fn installed_match<'a>(
    uuid: Option<uuid::Uuid>,
    name: &str,
    version: &str,
    installed: &'a [Installed],
) -> Option<&'a Installed> {
    installed
        .iter()
        .find(|i| (uuid.is_some() && i.uuid == uuid) || (i.name == name && i.version == version))
}

/// Record which rows the local image store has (see [`installed_match`]).
/// A row with no UUID (no sha256 published) takes the installed image's,
/// so `avail` shows it and `import <uuid>` finds the row by it.
pub fn mark_installed(rows: &mut [Available], installed: &[Installed]) {
    for row in rows {
        let found = installed_match(row.uuid, &row.name, &row.version, installed);
        row.installed = Some(found.is_some());
        if row.uuid.is_none() {
            row.uuid = found.and_then(|i| i.uuid);
            row.uuid_from_store = row.uuid.is_some();
        }
    }
}

/// `rows` as a table in the style of `imgadm avail`. An INSTALLED column
/// (`yes` or `no`) is added when the rows say whether they are installed.
/// Every row has every field, `-` when empty, so that scripts splitting on
/// whitespace find each column in the same place.
pub fn avail_table(rows: &[Available], header: bool) -> String {
    let known = rows.iter().any(|r| r.installed.is_some());
    let mut lines: Vec<Vec<String>> = Vec::new();
    if header {
        let mut names = vec!["UUID", "NAME", "VERSION", "OS", "ALIASES", "CHECK"];
        if known {
            names.push("INSTALLED");
        }
        lines.push(names.into_iter().map(str::to_string).collect());
    }
    for row in rows {
        let mut cells = vec![
            row.uuid.map_or("-".to_string(), |u| u.to_string()),
            row.name.clone(),
            row.version.clone(),
            row.os.clone(),
            if row.aliases.is_empty() {
                "-".to_string()
            } else {
                row.aliases.join(",")
            },
            row.check.as_str().to_string(),
        ];
        if known {
            let mark = if row.installed == Some(true) {
                "yes"
            } else {
                "no"
            };
            cells.push(mark.to_string());
        }
        lines.push(cells);
    }
    let mut widths = vec![0; if known { 7 } else { 6 }];
    for line in &lines {
        for (width, cell) in widths.iter_mut().zip(line) {
            *width = (*width).max(cell.len());
        }
    }
    let mut out = String::new();
    for line in &lines {
        let cells: Vec<String> = line
            .iter()
            .zip(&widths)
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
                "installed": r.installed,
                "nocloud": r.nocloud,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// An image without cloud-init's NoCloud datasource (the SmartOS USB
    /// image) is not described as one, and is not assumed to take SSH
    /// keys from metadata unless the index says it does.
    #[test]
    fn an_image_without_nocloud_is_not_described_as_one() -> anyhow::Result<()> {
        let rocky = rocky9();
        let (build, artifact) = choose_artifact(&rocky, "x86_64", None)?;
        let info = image_info(&rocky_distro(), &rocky.release, build, artifact);
        assert!(info.description.contains("NoCloud"), "{}", info.description);
        assert!(info.ssh_key);

        let plain = edited(&rocky, |a| {
            a["datasources"] = serde_json::json!([]);
            a["ssh_key_injection"] = serde_json::Value::Null;
        });
        let info = image_info(&rocky_distro(), &rocky.release, build, &plain);
        assert!(
            !info.description.contains("NoCloud compatible"),
            "{}",
            info.description
        );
        assert!(
            info.description
                .contains("without cloud-init's NoCloud datasource"),
            "{}",
            info.description
        );
        assert!(!info.ssh_key);

        let unknown = edited(&rocky, |a| a["ssh_key_injection"] = serde_json::Value::Null);
        let info = image_info(&rocky_distro(), &rocky.release, build, &unknown);
        assert!(
            info.ssh_key,
            "a NoCloud image takes keys unless the index says not"
        );
        let refuses = edited(&rocky, |a| a["ssh_key_injection"] = false.into());
        let info = image_info(&rocky_distro(), &rocky.release, build, &refuses);
        assert!(!info.ssh_key);
        Ok(())
    }

    /// `avail <distro>` still lists such an image, with a note saying what
    /// it lacks; a plain `avail` does not repeat the note on every run.
    #[test]
    fn avail_notes_an_image_without_nocloud() -> anyhow::Result<()> {
        let mut json = serde_json::to_value(images())?;
        let images = json["images"]
            .as_array_mut()
            .unwrap_or_else(|| panic!("images is a list"));
        let rocky = images
            .iter_mut()
            .find(|i| i["distro"]["id"] == "rocky")
            .unwrap_or_else(|| panic!("the examples have rocky"));
        rocky["build"]["artifacts"][0]["datasources"] = serde_json::json!([]);
        let rocky_release = rocky["release"]["id"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let list: client::types::ImageList = serde_json::from_value(json)?;
        let (rows, notes) = avail_rows(&list, Some("rocky"))?;
        assert!(rows.iter().any(|r| r.release == rocky_release));
        assert!(
            notes
                .iter()
                .any(|n| n.contains(&format!("rocky {rocky_release}"))
                    && n.contains("does not use cloud-init's NoCloud datasource")),
            "{notes:?}"
        );
        let (_, unfiltered) = avail_rows(&list, None)?;
        assert!(unfiltered.is_empty(), "{unfiltered:?}");
        let json = avail_json(&rows);
        let rocky_json = json
            .as_array()
            .into_iter()
            .flatten()
            .find(|r| r["release"] == rocky_release.as_str())
            .unwrap_or_else(|| panic!("rocky is in -j"));
        assert_eq!(rocky_json["nocloud"], serde_json::json!(false));
        Ok(())
    }

    /// Directories are made and written as root after these ids, so ids
    /// from the index (which `--index` can point anywhere) must follow the
    /// id rule; each is checked on its own and named when it does not.
    #[test]
    fn ids_that_would_escape_the_directory_are_refused() -> anyhow::Result<()> {
        let rocky = rocky9();
        let (build, artifact) = choose_artifact(&rocky, "x86_64", None)?;
        check_ids(&rocky_distro(), &rocky.release, build, artifact)?;
        let error = |r: anyhow::Result<()>| r.err().map(|e| format!("{e:#}")).unwrap_or_default();
        let mut bad = build.clone();
        for id in ["../../etc", "a/b", ""] {
            bad.build = id.to_string();
            let err = error(check_ids(&rocky_distro(), &rocky.release, &bad, artifact));
            assert!(err.contains("build id"), "{id:?}: {err}");
        }
        let distro = Distro {
            id: "../rocky".to_string(),
            ..rocky_distro()
        };
        let err = error(check_ids(&distro, &rocky.release, build, artifact));
        assert!(err.contains("distro id"), "{err}");
        let err = error(check_ids(&rocky_distro(), "..", build, artifact));
        assert!(err.contains("release id"), "{err}");
        let variant = edited(&rocky, |a| a["variant"] = "base/x".into());
        let err = error(check_ids(&rocky_distro(), &rocky.release, build, &variant));
        assert!(err.contains("variant"), "{err}");
        // A build id and a variant may each use the whole length.
        let mut long = build.clone();
        long.build = "1".repeat(64);
        let long_variant = edited(&rocky, |a| a["variant"] = "v".repeat(64).into());
        check_ids(&rocky_distro(), &rocky.release, &long, &long_variant)?;
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

    /// `import` reads a distro from v1/images.json alone: its releases with
    /// their titles and aliases, in order, each with its builds.
    #[test]
    fn a_distro_is_read_from_the_image_list() -> anyhow::Result<()> {
        let (distro, builds) = distro_from_images(&images(), "ubuntu")?;
        assert_eq!(distro.id, "ubuntu");
        let releases: Vec<&str> = distro.releases.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(releases, ["resolute", "noble", "jammy"]);
        assert_eq!(alias_names(&distro.releases[0]), ["latest", "lts"]);
        assert_eq!(builds.len(), 3);
        assert_eq!(builds[1].release, "noble");
        assert_eq!(builds[1].builds[0].build, "20260926");
        // The same choices and resolution as from the distro's own file.
        assert_eq!(release_choices(&distro), release_choices(&ubuntu_distro()));
        assert_eq!(release_request(&distro, "lts")?, Requested::Alias("lts"));
        let err = distro_from_images(&images(), "nosuch")
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(err.contains("ubuntu, rocky, talos"), "{err}");
        Ok(())
    }

    /// An alias or release name picks that release's builds.
    #[test]
    fn a_requested_release_picks_its_builds() -> anyhow::Result<()> {
        let (distro, builds) = distro_from_images(&images(), "ubuntu")?;
        let pick = |name| -> anyhow::Result<String> {
            let request = release_request(&distro, name)?;
            Ok(requested_builds(&distro, &builds, request)?.release.clone())
        };
        assert_eq!(pick("latest")?, "resolute");
        assert_eq!(pick("lts")?, "resolute");
        assert_eq!(pick("jammy")?, "jammy");
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

    /// What `imgadm list -j` prints: each image's stored record, with
    /// its manifest.
    const IMGADM_LIST: &str = r#"[
      {"manifest": {"v": 2, "uuid": "006d99ef-603a-551d-b179-c56d3f6e1c32",
        "name": "ubuntu-resolute-nocloud", "version": "20260927"},
       "zpool": "zones", "source": "https://images.smartos.org"},
      {"manifest": {"v": 2, "uuid": "11111111-2222-3333-4444-555555555555",
        "name": "ubuntu-jammy-nocloud", "version": "20261004"},
       "zpool": "zones"},
      {"manifest": {"v": 2, "uuid": "22222222-2222-3333-4444-555555555555",
        "name": "ubuntu-noble-nocloud", "version": "20200101"},
       "zpool": "zones"}
    ]"#;

    #[test]
    fn the_image_store_listing_is_read() -> anyhow::Result<()> {
        let installed = parse_imgadm_list(IMGADM_LIST)?;
        assert_eq!(installed.len(), 3);
        assert_eq!(installed[1].name, "ubuntu-jammy-nocloud");
        assert_eq!(installed[1].version, "20261004");
        assert!(parse_imgadm_list("not json").is_err());
        Ok(())
    }

    /// An image is installed if the store has its UUID, or (when its UUID
    /// is not known before the download) its name and version; another
    /// version of the same name is not this image.
    #[test]
    fn installed_images_are_marked() -> anyhow::Result<()> {
        let (mut rows, _) = avail_rows(&images(), Some("ubuntu"))?;
        mark_installed(&mut rows, &parse_imgadm_list(IMGADM_LIST)?);
        let marks: Vec<(&str, Option<bool>)> = rows
            .iter()
            .map(|r| (r.name.as_str(), r.installed))
            .collect();
        assert_eq!(
            marks,
            [
                ("ubuntu-resolute-nocloud", Some(true)),
                ("ubuntu-noble-nocloud", Some(false)),
                ("ubuntu-jammy-nocloud", Some(true)),
            ]
        );
        Ok(())
    }

    /// An installed image gives `avail` the UUID the index cannot: one
    /// whose vendor publishes no sha256 (Talos here, Alpine in practice)
    /// has none before its download. A UUID the index gives is kept.
    #[test]
    fn an_installed_image_gives_a_uuid_the_index_cannot() -> anyhow::Result<()> {
        let (mut rows, _) = avail_rows(&images(), Some("talos"))?;
        assert_eq!(rows[0].uuid, None);
        let uuid = uuid::Uuid::parse_str("33333333-2222-3333-4444-555555555555")?;
        let installed = [Installed {
            uuid: Some(uuid),
            name: rows[0].name.clone(),
            version: rows[0].version.clone(),
        }];
        mark_installed(&mut rows, &installed);
        assert_eq!((rows[0].installed, rows[0].uuid), (Some(true), Some(uuid)));
        assert!(rows[0].uuid_from_store);
        assert_eq!(avail_json(&rows)[0]["uuid"], uuid.to_string());
        // So `import <uuid>` finds the row by the UUID `avail` showed.
        assert_eq!(
            find_by_uuid(&rows, uuid).map(|r| r.name.as_str()),
            Some(rows[0].name.as_str())
        );

        let (mut ubuntu, _) = avail_rows(&images(), Some("ubuntu"))?;
        let before: Vec<_> = ubuntu.iter().map(|r| r.uuid).collect();
        mark_installed(&mut ubuntu, &parse_imgadm_list(IMGADM_LIST)?);
        let after: Vec<_> = ubuntu.iter().map(|r| r.uuid).collect();
        assert_eq!(before, after);
        assert!(ubuntu.iter().all(|r| !r.uuid_from_store));
        Ok(())
    }

    /// `import <uuid>` accepts every UUID `avail` lists, so a UUID it
    /// does not find is simply not one of them.
    #[test]
    fn a_uuid_avail_does_not_list_is_refused_plainly() {
        let uuid = uuid::Uuid::nil();
        let message = uuid_not_listed(uuid);
        assert!(message.contains(&uuid.to_string()), "{message}");
        assert!(message.contains("cloud-image avail"), "{message}");
        assert!(!message.contains("until it is downloaded"), "{message}");
    }

    /// `import` finds the installed image it would produce the same way
    /// `avail` marks it, so it can stop before downloading it again.
    #[test]
    fn an_import_finds_the_image_already_installed() -> anyhow::Result<()> {
        let installed = parse_imgadm_list(IMGADM_LIST)?;
        let resolute = uuid::Uuid::parse_str("006d99ef-603a-551d-b179-c56d3f6e1c32")?;
        let by_uuid = installed_match(Some(resolute), "other-name", "1", &installed);
        assert_eq!(by_uuid.and_then(|i| i.uuid), Some(resolute));
        let by_name = installed_match(None, "ubuntu-jammy-nocloud", "20261004", &installed);
        assert_eq!(
            by_name.map(|i| i.name.as_str()),
            Some("ubuntu-jammy-nocloud")
        );
        assert!(installed_match(None, "ubuntu-noble-nocloud", "20260927", &installed).is_none());
        Ok(())
    }

    /// The column appears only when the store was read, so off SmartOS
    /// the table is as before.
    #[test]
    fn the_installed_column_appears_when_known() -> anyhow::Result<()> {
        let (mut rows, _) = avail_rows(&images(), Some("ubuntu"))?;
        assert!(!avail_table(&rows, true).contains("INSTALLED"));
        assert_eq!(avail_json(&rows)[0]["installed"], serde_json::Value::Null);
        mark_installed(&mut rows, &parse_imgadm_list(IMGADM_LIST)?);
        let table = avail_table(&rows, true);
        let lines: Vec<&str> = table.lines().collect();
        assert!(lines[0].ends_with("CHECK   INSTALLED"), "{table}");
        assert!(lines[1].ends_with("vendor  yes"), "{table}");
        assert!(lines[2].ends_with("vendor  no"), "{table}");
        assert_eq!(avail_json(&rows)[0]["installed"], serde_json::json!(true));
        assert_eq!(avail_json(&rows)[1]["installed"], serde_json::json!(false));
        Ok(())
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

    /// Every row has the same number of fields, so `awk` or `cut` find the
    /// same column in each: an empty cell is `-`.
    #[test]
    fn every_row_has_every_field() -> anyhow::Result<()> {
        let (mut rows, _) = avail_rows(&images(), None)?;
        for table in [avail_table(&rows, false), {
            mark_installed(&mut rows, &parse_imgadm_list(IMGADM_LIST)?);
            avail_table(&rows, false)
        }] {
            let counts: Vec<usize> = table
                .lines()
                .map(|l| l.split_whitespace().count())
                .collect();
            assert!(
                counts.windows(2).all(|w| w[0] == w[1]),
                "{counts:?}\n{table}"
            );
        }
        let noble = avail_table(&rows, false)
            .lines()
            .find(|l| l.contains("ubuntu-noble-nocloud"))
            .map(str::to_string)
            .unwrap_or_default();
        assert!(noble.contains("  -  "), "{noble}");
        assert!(noble.ends_with("vendor  no"), "{noble}");
        Ok(())
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

    /// `import <uuid>` installs that image or nothing: if the index moved
    /// on between the lookup and the import, the UUIDs differ.
    #[test]
    fn an_import_by_uuid_must_get_that_uuid() {
        let asked = uuid::Uuid::from_u128(1);
        let other = uuid::Uuid::from_u128(2);
        assert!(confirm_requested_uuid(None, Some(other)).is_ok());
        assert!(confirm_requested_uuid(Some(asked), Some(asked)).is_ok());
        let err = confirm_requested_uuid(Some(asked), Some(other))
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(
            err.contains(&asked.to_string()) && err.contains(&other.to_string()),
            "{err}"
        );
        assert!(confirm_requested_uuid(Some(asked), None).is_err());
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

    /// A digest in an algorithm added to the index after this client
    /// cannot be checked, so it is left out rather than refused.
    #[test]
    fn a_digest_in_an_unknown_algorithm_is_left_out() -> anyhow::Result<()> {
        let ubuntu = ubuntu();
        let artifact = edited(&ubuntu, |a| {
            a["integrity"]["digests"][0]["algorithm"] = "blake2b".into()
        });
        let digests = index_digests(&artifact)?;
        assert!(digests.is_empty());
        assert!(needs_allow_unverified(&digests));
        assert_eq!(unknown_digest_algorithms(&artifact), ["blake2b"]);
        Ok(())
    }

    /// Refusing an unverified import says why: a digest this client cannot
    /// check calls for a newer client, not for trusting TLS alone.
    #[test]
    fn an_unverified_import_says_why() -> anyhow::Result<()> {
        let ubuntu = ubuntu();
        let (_, artifact) = choose_artifact(&ubuntu, "x86_64", None)?;
        assert!(unknown_digest_algorithms(artifact).is_empty());

        let none = why_unverified("talos", &[], &[]);
        assert!(none.contains("publishes no digest"), "{none}");
        assert!(none.contains("--allow-unverified"), "{none}");

        let newer = why_unverified("ubuntu", &[], &["blake2b".to_string()]);
        assert!(!newer.contains("publishes no digest"), "{newer}");
        assert!(newer.contains("blake2b"), "{newer}");
        assert!(newer.contains("cloud-image self-update"), "{newer}");

        let document = edited(&ubuntu, |a| {
            a["integrity"]["digests"][0]["source"]["format"] = "vendor_document".into()
        });
        let index_only = why_unverified("ubuntu", &index_digests(&document)?, &[]);
        assert!(index_only.contains("no generic layout"), "{index_only}");
        Ok(())
    }

    /// `avail` notes a digest this client cannot check.
    #[test]
    fn avail_notes_a_digest_this_client_cannot_check() -> anyhow::Result<()> {
        let mut json = serde_json::to_value(images())?;
        for image in json["images"].as_array_mut().into_iter().flatten() {
            if image["distro"]["id"] == "ubuntu" {
                image["build"]["artifacts"][0]["integrity"]["digests"][0]["algorithm"] =
                    "blake2b".into();
            }
        }
        let list: client::types::ImageList = serde_json::from_value(json)?;
        let (_, notes) = avail_rows(&list, None)?;
        assert!(
            notes
                .iter()
                .any(|n| n.contains("blake2b") && n.contains("cloud-image self-update")),
            "{notes:?}"
        );
        Ok(())
    }

    /// A location of a kind added after this client is not the primary
    /// one, and does not stop the client finding the primary.
    #[test]
    fn a_location_of_an_unknown_kind_is_not_primary() -> anyhow::Result<()> {
        let ubuntu = ubuntu();
        let only_unknown = edited(&ubuntu, |a| a["locations"][0]["kind"] = "cdn".into());
        assert!(primary_url(&only_unknown).is_err());
        let both = edited(&ubuntu, |a| {
            let primary = a["locations"][0].clone();
            let mut cdn = primary.clone();
            cdn["kind"] = "cdn".into();
            cdn["url"] = "https://cdn.example/x.img".into();
            a["locations"] = serde_json::json!([cdn, primary]);
        });
        assert!(
            primary_url(&both)?
                .as_str()
                .ends_with("server-cloudimg-amd64.img")
        );
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
