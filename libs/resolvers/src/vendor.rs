// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Vendor profiles. Each vendor implements `VendorProfile`, which knows
//! how to list its resolvable releases ([`VendorProfile::list_versions`])
//! and resolve a release token (series name, version, or `latest`) into
//! a concrete URL, format, manifest metadata, and verifier
//! ([`VendorProfile::resolve`]).

use anyhow::{Context, Result};
use async_trait::async_trait;
use url::Url;

use super::verify::{ChecksumDocument, Sha256Pinned, SumsStyle, Verifier};

pub mod alma;
pub mod alpine;
pub mod arch;
pub mod centosstream;
pub mod debian;
pub mod dirlist;
pub mod fedora;
pub mod freebsd;
pub mod omnios;
pub mod openbsd;
pub mod opensuse;
pub mod oracle;
pub mod rocky;
pub mod smartos;
pub mod talos;
pub mod ubuntu;

/// Built-in vendor profiles. Driven by clap's `ValueEnum` so the CLI
/// help auto-lists supported vendors and validates the argument
/// before any I/O. The variant→string mapping is derived from
/// `serde::Serialize` (kebab-case), and `Display` delegates to
/// the crate-local [`crate::enum_to_display`], so adding a vendor is a single-line
/// enum-variant addition with no string-matching boilerplate.
#[derive(clap::ValueEnum, serde::Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Vendor {
    Alma,
    Alpine,
    Arch,
    CentosStream,
    Debian,
    Fedora,
    Freebsd,
    Omnios,
    Openbsd,
    Opensuse,
    Oracle,
    Rocky,
    Smartos,
    Talos,
    Ubuntu,
}

impl Vendor {
    /// Every built-in vendor, in display order. The single source of
    /// truth for [`all_vendors`] and any caller that needs to enumerate
    /// the catalog.
    pub const ALL: [Vendor; 15] = [
        Vendor::Ubuntu,
        Vendor::Debian,
        Vendor::Rocky,
        Vendor::Alma,
        Vendor::CentosStream,
        Vendor::Fedora,
        Vendor::Oracle,
        Vendor::Opensuse,
        Vendor::Alpine,
        Vendor::Arch,
        Vendor::Freebsd,
        Vendor::Openbsd,
        Vendor::Omnios,
        Vendor::Smartos,
        Vendor::Talos,
    ];

    /// Kebab-case identifier (the same string clap/serde use, e.g.
    /// `centos-stream`). Stable wire id for the distributions endpoint.
    pub fn id(&self) -> String {
        crate::enum_to_display(self)
    }

    /// Human-facing display label.
    pub fn label(&self) -> &'static str {
        match self {
            Vendor::Alma => "AlmaLinux",
            Vendor::Alpine => "Alpine Linux",
            Vendor::Arch => "Arch Linux",
            Vendor::CentosStream => "CentOS Stream",
            Vendor::Debian => "Debian",
            Vendor::Fedora => "Fedora",
            Vendor::Freebsd => "FreeBSD",
            Vendor::Omnios => "OmniOS",
            Vendor::Openbsd => "OpenBSD",
            Vendor::Opensuse => "openSUSE Leap",
            Vendor::Oracle => "Oracle Linux",
            Vendor::Rocky => "Rocky Linux",
            Vendor::Smartos => "SmartOS",
            Vendor::Talos => "Talos Linux",
            Vendor::Ubuntu => "Ubuntu",
        }
    }

    /// The image flavour this vendor's profile resolves to, named from
    /// the vendor's own file naming (e.g. Debian `genericcloud`, Rocky
    /// `base` rather than `lvm`).
    pub fn default_variant(&self) -> &'static str {
        match self {
            Vendor::Alma => "genericcloud",
            Vendor::Alpine => "cloudinit",
            Vendor::Arch => "cloudimg",
            Vendor::CentosStream => "genericcloud",
            Vendor::Debian => "genericcloud",
            Vendor::Fedora => "cloud-base-generic",
            Vendor::Freebsd => "basic-cloudinit-zfs",
            Vendor::Omnios => "cloud",
            Vendor::Openbsd => "min",
            Vendor::Opensuse => "minimal-vm-cloud",
            Vendor::Oracle => "kvm",
            Vendor::Rocky => "base",
            Vendor::Smartos => "usb",
            Vendor::Talos => "nocloud",
            Vendor::Ubuntu => "server",
        }
    }

    /// Manifest `os` family this vendor produces (`linux`, `bsd`,
    /// `illumos`). Matches the `os` field the resolver sets on
    /// [`ResolvedImage`].
    pub fn os(&self) -> &'static str {
        match self {
            Vendor::Freebsd | Vendor::Openbsd => "bsd",
            Vendor::Omnios | Vendor::Smartos => "illumos",
            _ => "linux",
        }
    }
}

impl std::fmt::Display for Vendor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&crate::enum_to_display(self))
    }
}

/// Display metadata for one built-in vendor (kebab id + human label +
/// os family), for clients that offer a choice of vendors.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct VendorInfo {
    /// Kebab-case wire id (e.g. `centos-stream`).
    pub id: String,
    /// Human-facing label (e.g. `CentOS Stream`).
    pub label: String,
    /// Manifest `os` family (`linux`, `bsd`, `illumos`).
    pub os: String,
}

/// The display catalog of every built-in vendor, in display order.
pub fn all_vendors() -> Vec<VendorInfo> {
    Vendor::ALL
        .iter()
        .map(|v| VendorInfo {
            id: v.id(),
            label: v.label().to_string(),
            os: v.os().to_string(),
        })
        .collect()
}

/// One resolvable release in a vendor's catalog. The `token` is exactly
/// what [`VendorProfile::resolve`] accepts; the remaining fields are
/// display sugar for a version picker. `supported` is false for
/// EOL / pre-release / dev channels the vendor still serves; callers
/// decide whether to offer those.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct VersionEntry {
    /// Release token to feed back into `resolve` (e.g. `noble`, `9`,
    /// `stable`, `latest`).
    pub token: String,
    /// Canonical short series name (e.g. `noble`, `rocky9`, `leap16.0`).
    pub series: String,
    /// Vendor version string (e.g. `24.04`, `9`, `15.0`).
    pub version: String,
    /// Human-facing title (e.g. `24.04 LTS Noble Numbat`).
    pub title: String,
    /// Whether the vendor still maintains this release.
    pub supported: bool,
    /// The vendor's end-of-support date for this release, when the
    /// vendor's feed gives one.
    pub eol_date: Option<chrono::NaiveDate>,
    /// The vendor labels this release long-term support (e.g. Ubuntu's
    /// LTS releases, OmniOS's `lts` channel).
    pub lts: bool,
    /// For a development channel or pre-release, the vendor's name for
    /// it (e.g. OmniOS `bloody`, Fedora `beta`, openSUSE `rc`).
    pub dev: Option<String>,
    /// The entry is a moving channel (e.g. OmniOS `stable`, or `latest`)
    /// rather than a release. The release it currently points at is
    /// named by the resolved image's `facts.release`.
    pub channel: bool,
}

#[derive(Debug, Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceFormat {
    Qcow2,
    Xz,
    Raw,
    /// VMDK (VMware Virtual Disk). Used by OmniOS's cloud images.
    /// The release-resolution path is wired up; the conversion step
    /// is deferred pending a vendored vmdk reader.
    Vmdk,
    /// gzipped raw disk image. Used by SmartOS
    /// (`smartos-<rel>-USB.img.gz`); a client can stream a gzip
    /// decoder straight onto the target disk, no intermediate file.
    RawGz,
}

pub struct ResolvedImage {
    pub url: Url,
    pub format: SourceFormat,
    /// Image OS for the manifest (`linux`, `bsd`, ...).
    pub os: String,
    /// Canonical short release name (e.g. `noble`). Used in output
    /// filenames and the manifest `name` field.
    pub series: String,
    /// Vendor-chosen version string (often a date stamp). Used as the
    /// manifest `version` field.
    pub version: String,
    pub description: String,
    pub homepage: Url,
    pub ssh_key: bool,
    pub verifier: Box<dyn Verifier>,
    /// Vendors that get the sha256 from their metadata feed (e.g.
    /// Ubuntu Simple Streams) populate this so a caller knows the
    /// expected hash (and anything derived from it) without
    /// downloading anything. Vendors whose verifier fetches the hash
    /// at verification time leave this `None`.
    pub expected_sha256: Option<String>,
    /// Further facts about the image the vendor publishes, when the
    /// profile reads them.
    pub facts: ImageFacts,
}

/// Optional facts about a resolved image. Each is set only from what
/// the vendor itself publishes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageFacts {
    /// The point release the image belongs to (e.g. `9.8`), for vendors
    /// whose releases are majors with point releases.
    pub point_release: Option<String>,
    /// For an image resolved from a channel, the release the channel
    /// currently points at (e.g. OmniOS `r151058`, Arch `rolling`).
    pub release: Option<String>,
    /// When the vendor says it published the image.
    pub published_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Where the vendor's signatures should be, by its naming
    /// conventions. Not checked by the profile; a caller should confirm
    /// the vendor serves each one before relying on it.
    pub signatures: Vec<SignatureRef>,
}

/// A vendor OpenPGP signature over a checksum document or the image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureRef {
    pub url: Url,
    pub kind: SignatureKind,
    /// The document the signature covers (for a clear-signed document,
    /// the same as `url`).
    pub signs: Url,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureKind {
    Detached,
    Clearsigned,
}

/// A detached signature at `signed` with `suffix` appended
/// (e.g. `CHECKSUM` and `.asc`).
pub(super) fn detached_signature(signed: &str, suffix: &str) -> Result<SignatureRef> {
    let signs = Url::parse(signed).with_context(|| format!("signed document url {signed}"))?;
    let url = Url::parse(&format!("{signed}{suffix}"))
        .with_context(|| format!("signature url {signed}{suffix}"))?;
    Ok(SignatureRef {
        url,
        kind: SignatureKind::Detached,
        signs,
    })
}

#[async_trait]
pub trait VendorProfile: Send + Sync {
    // Reserved for diagnostics/logging by callers; not yet referenced
    // in this repository.
    #[allow(dead_code)]
    fn name(&self) -> &str;

    /// List the releases this vendor can resolve, newest-first. Reads
    /// only the SAME hardcoded distro index the resolver already uses
    /// (no new hosts). Offline/rolling vendors return their accepted
    /// channel tokens (e.g. `stable`/`lts`/`bloody`, `latest`) without
    /// a network read.
    async fn list_versions(&self, http: &reqwest::Client) -> Result<Vec<VersionEntry>>;

    /// Validate the release token, then resolve it. Token validation
    /// lives at this boundary (not just in the admin route that calls
    /// `list_versions`) so the bounded-charset guard travels with every
    /// caller: the per-vendor [`VendorProfile::resolve_release`] impls
    /// interpolate the token straight into URL templates, so an
    /// unbounded token is a path-injection / second-request vector.
    async fn resolve(&self, release: &str, http: &reqwest::Client) -> Result<ResolvedImage> {
        validate_version_token(release)?;
        self.resolve_release(release, http).await
    }

    /// Resolve a release token into a concrete image. Callers must go
    /// through [`VendorProfile::resolve`], which validates the token
    /// before this runs.
    async fn resolve_release(&self, release: &str, http: &reqwest::Client)
    -> Result<ResolvedImage>;
}

/// Maximum accepted release-token length.
const MAX_VERSION_TOKEN_LEN: usize = 64;

/// Validate a release token before it reaches a resolver's URL template:
/// bounded `[A-Za-z0-9._-]{1,64}` with an explicit `..` reject, so no
/// path separator or dot-dot traversal can be smuggled into the
/// per-vendor URL the resolver builds. A real version token (series
/// name, version string, or channel like `latest`/`stable`) never needs
/// anything outside this set. Enforced by [`VendorProfile::resolve`].
pub fn validate_version_token(token: &str) -> Result<()> {
    let ok = !token.is_empty()
        && token.len() <= MAX_VERSION_TOKEN_LEN
        && !token.contains("..")
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        anyhow::bail!(
            "invalid version token {token:?}: expected [A-Za-z0-9._-]{{1,64}} with no '..'"
        );
    }
}

/// Builder for the common "linux qcow2 with vendor-pinned sha256"
/// `ResolvedImage` shape used by the RHEL-derivative profiles
/// (alma, rocky, oracle, centosstream, fedora, opensuse). All of them
/// share `SourceFormat::Qcow2`, `os = "linux"`, `ssh_key = true`, and
/// a `Sha256Pinned` verifier driven by a hash that release discovery
/// already extracted; the only per-vendor variation is the series,
/// version, description, and homepage strings.
pub(super) struct PinnedQcow2 {
    pub url: Url,
    pub series: String,
    pub version: String,
    pub description: String,
    pub homepage: &'static str,
    pub sha256: String,
    /// The vendor document the hash was read from.
    pub document: ChecksumDocument,
    /// The point release the image belongs to, when the vendor has them.
    pub point_release: Option<String>,
    /// Where the vendor's signatures should be.
    pub signatures: Vec<SignatureRef>,
}

impl PinnedQcow2 {
    pub fn into_resolved(self, vendor_label: &str) -> Result<ResolvedImage> {
        Ok(ResolvedImage {
            url: self.url,
            format: SourceFormat::Qcow2,
            os: "linux".to_string(),
            series: self.series,
            version: self.version,
            description: self.description,
            homepage: Url::parse(self.homepage)
                .with_context(|| format!("{vendor_label} homepage url"))?,
            ssh_key: true,
            verifier: Box::new(Sha256Pinned::from_document(
                self.sha256.clone(),
                self.document,
            )),
            expected_sha256: Some(self.sha256),
            facts: ImageFacts {
                point_release: self.point_release,
                signatures: self.signatures,
                ..ImageFacts::default()
            },
        })
    }
}

/// A vendor checksum document for a pinned hash, from the URL the
/// profile read it from.
pub(super) fn checksum_document(
    url: &str,
    filename: &str,
    style: SumsStyle,
) -> Result<ChecksumDocument> {
    Ok(ChecksumDocument {
        url: Url::parse(url).with_context(|| format!("checksum document url {url}"))?,
        filename: filename.to_string(),
        style,
    })
}

/// The point release in a build id like `9.8-20260525.0` (`9.8`).
pub(super) fn point_release_of(build: &str) -> Option<String> {
    let (point, _) = build.split_once('-')?;
    (point.contains('.') && point.chars().all(|c| c.is_ascii_digit() || c == '.'))
        .then(|| point.to_string())
}

pub fn lookup(vendor: Vendor) -> Box<dyn VendorProfile> {
    match vendor {
        Vendor::Alma => Box::new(alma::Alma),
        Vendor::Alpine => Box::new(alpine::Alpine),
        Vendor::Arch => Box::new(arch::Arch),
        Vendor::CentosStream => Box::new(centosstream::CentosStream),
        Vendor::Debian => Box::new(debian::Debian),
        Vendor::Fedora => Box::new(fedora::Fedora),
        Vendor::Freebsd => Box::new(freebsd::FreeBsd),
        Vendor::Omnios => Box::new(omnios::Omnios),
        Vendor::Openbsd => Box::new(openbsd::OpenBsd),
        Vendor::Opensuse => Box::new(opensuse::OpenSuse),
        Vendor::Oracle => Box::new(oracle::Oracle),
        Vendor::Rocky => Box::new(rocky::Rocky),
        Vendor::Smartos => Box::new(smartos::Smartos),
        Vendor::Talos => Box::new(talos::Talos),
        Vendor::Ubuntu => Box::new(ubuntu::Ubuntu),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default image flavour each built-in profile resolves to,
    /// named from the vendor's own file naming (see the resolved URLs
    /// in each profile).
    #[test]
    fn default_variant_names_the_resolved_flavour() {
        let expected = [
            (Vendor::Alma, "genericcloud"),
            (Vendor::Alpine, "cloudinit"),
            (Vendor::Arch, "cloudimg"),
            (Vendor::CentosStream, "genericcloud"),
            (Vendor::Debian, "genericcloud"),
            (Vendor::Fedora, "cloud-base-generic"),
            (Vendor::Freebsd, "basic-cloudinit-zfs"),
            (Vendor::Omnios, "cloud"),
            (Vendor::Openbsd, "min"),
            (Vendor::Opensuse, "minimal-vm-cloud"),
            (Vendor::Oracle, "kvm"),
            (Vendor::Rocky, "base"),
            (Vendor::Smartos, "usb"),
            (Vendor::Talos, "nocloud"),
            (Vendor::Ubuntu, "server"),
        ];
        assert_eq!(expected.len(), Vendor::ALL.len());
        for (vendor, variant) in expected {
            assert_eq!(vendor.default_variant(), variant, "{vendor}");
        }
    }

    #[test]
    fn all_vendors_covers_every_variant() {
        let v = all_vendors();
        assert_eq!(v.len(), Vendor::ALL.len());
        // ids are non-empty kebab strings; os is one of the known
        // families.
        for info in &v {
            assert!(!info.id.is_empty());
            assert!(!info.label.is_empty());
            assert!(
                matches!(info.os.as_str(), "linux" | "bsd" | "illumos"),
                "unexpected os {:?}",
                info.os
            );
        }
        // Spot-check the kebab id with a hyphen survives.
        assert!(v.iter().any(|i| i.id == "centos-stream"));
        assert!(v.iter().any(|i| i.id == "ubuntu" && i.os == "linux"));
        assert!(v.iter().any(|i| i.id == "freebsd" && i.os == "bsd"));
        assert!(v.iter().any(|i| i.id == "omnios" && i.os == "illumos"));
    }

    #[test]
    fn pinned_qcow2_carries_document_and_point_release() {
        let doc = crate::verify::ChecksumDocument {
            url: Url::parse("https://v.example/img.CHECKSUM").unwrap_or_else(|e| panic!("{e}")),
            filename: "img.qcow2".to_string(),
            style: crate::verify::SumsStyle::Bsd,
        };
        let resolved = PinnedQcow2 {
            url: Url::parse("https://v.example/img.qcow2").unwrap_or_else(|e| panic!("{e}")),
            series: "s".to_string(),
            version: "9.8-1".to_string(),
            description: String::new(),
            homepage: "https://v.example/",
            sha256: "abc".to_string(),
            document: doc.clone(),
            point_release: Some("9.8".to_string()),
            signatures: Vec::new(),
        }
        .into_resolved("test")
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(resolved.facts.point_release.as_deref(), Some("9.8"));
        assert_eq!(
            resolved.verifier.checksum_source(),
            crate::verify::ChecksumSource::Pinned {
                algorithm: crate::verify::HashAlgorithm::Sha256,
                hash: "abc".to_string(),
                document: Some(doc),
            }
        );
    }

    #[test]
    fn point_release_of_reads_dotted_prefixes_only() {
        assert_eq!(point_release_of("9.8-20260525.0").as_deref(), Some("9.8"));
        assert_eq!(point_release_of("10.2-20260817.0").as_deref(), Some("10.2"));
        assert_eq!(point_release_of("20260930.0"), None);
        assert_eq!(point_release_of("44-1.7"), None);
        assert_eq!(point_release_of("r151058"), None);
    }

    #[test]
    fn validate_version_token_accepts_real_tokens() {
        for t in [
            "latest",
            "noble",
            "stable",
            "bloody",
            "9.4",
            "14.0",
            "v3.23",
            "centos-stream-9",
        ] {
            assert!(validate_version_token(t).is_ok(), "rejected {t:?}");
        }
    }

    #[test]
    fn validate_version_token_rejects_injection() {
        for t in [
            "",
            "../etc",
            "a/b",
            "..",
            "a b",
            "a:b",
            "a%2e",
            &"a".repeat(65),
        ] {
            assert!(validate_version_token(t).is_err(), "accepted {t:?}");
        }
    }

    /// The guard lives on the trait, so a crafted token is rejected
    /// before any per-vendor URL interpolation or network read — even
    /// when a caller invokes `resolve` directly.
    #[tokio::test]
    async fn resolve_rejects_bad_token_before_fetch() {
        let http = reqwest::Client::new();
        let err = match lookup(Vendor::Freebsd)
            .resolve("../../etc/passwd", &http)
            .await
        {
            Ok(_) => panic!("dot-dot token must be rejected"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("invalid version token"));
    }
}
