// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Checksum verification strategies for fetched upstream images.
//!
//! `Sha256SumsTls` and `Sha512SumsTls` fetch a vendor-published
//! `<HASH>SUMS`-style listing over TLS and match by filename — same
//! threat model as a TLS-fetched URL with the hash pinned in our own
//! repo. `Sha256Pinned` is for static profiles where the caller
//! already knows the expected digest.

use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;
use sha2::{Digest, Sha256, Sha512};
use tokio::io::AsyncReadExt;
use url::Url;

/// A `Verifier` checks the authenticity of a downloaded file. The
/// caller passes the file's sha256, which an import client computes
/// anyway (e.g. to derive a stable image UUID), so verifiers that work
/// in sha256 use it directly. Verifiers that
/// need a different hash function (e.g. SHA-512 for Debian) get the
/// file path and hash it themselves.
#[async_trait]
pub trait Verifier: Send + Sync {
    async fn verify(
        &self,
        file: &Path,
        file_sha256_hex: &str,
        http: &reqwest::Client,
    ) -> Result<()>;

    /// Where this verifier gets the expected hash. Lets the index
    /// publish the vendor's checksum document without downloading the
    /// image.
    fn checksum_source(&self) -> ChecksumSource;
}

/// Hash function a vendor checksum uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashAlgorithm {
    Sha256,
    Sha512,
}

/// Where a [`Verifier`] gets its expected hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChecksumSource {
    /// The hash was read at resolve time.
    Pinned {
        algorithm: HashAlgorithm,
        hash: String,
        /// The vendor document the hash was read from, when recorded.
        document: Option<ChecksumDocument>,
    },
    /// A vendor checksum document, read at verify time. `filename` is
    /// empty for [`SumsStyle::Bare`].
    Document {
        url: Url,
        filename: String,
        style: SumsStyle,
        algorithm: HashAlgorithm,
    },
    /// The vendor publishes no hash for this image.
    None { note: String },
}

/// A sha256 the resolver already read from a vendor document (e.g.
/// Ubuntu's Simple Streams JSON or a per-file checksum sidecar), so
/// nothing has to be fetched at verify time. `document` records which
/// vendor document the hash came from, when the profile says.
pub struct Sha256Pinned {
    pub hash: String,
    pub document: Option<ChecksumDocument>,
}

impl Sha256Pinned {
    pub fn new(hash: String) -> Self {
        Self {
            hash,
            document: None,
        }
    }

    /// A pinned hash together with the vendor document it was read from.
    pub fn from_document(hash: String, document: ChecksumDocument) -> Self {
        Self {
            hash,
            document: Some(document),
        }
    }
}

/// A vendor document that lists an image's hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChecksumDocument {
    pub url: Url,
    /// The name the image is listed under (empty for [`SumsStyle::Bare`]
    /// and [`SumsStyle::VendorDocument`]).
    pub filename: String,
    pub style: SumsStyle,
}

#[async_trait]
impl Verifier for Sha256Pinned {
    async fn verify(
        &self,
        _file: &Path,
        file_sha256_hex: &str,
        _http: &reqwest::Client,
    ) -> Result<()> {
        if file_sha256_hex != self.hash {
            anyhow::bail!(
                "sha256 mismatch\n  expected: {}\n  actual:   {file_sha256_hex}",
                self.hash
            );
        }
        eprintln!("Checksum OK: {file_sha256_hex}");
        Ok(())
    }

    fn checksum_source(&self) -> ChecksumSource {
        ChecksumSource::Pinned {
            algorithm: HashAlgorithm::Sha256,
            hash: self.hash.clone(),
            document: self.document.clone(),
        }
    }
}

pub struct Sha256SumsTls {
    pub sums_url: Url,
    pub filename: String,
}

impl Sha256SumsTls {
    pub fn new(sums_url: Url, filename: String) -> Self {
        Self { sums_url, filename }
    }
}

#[async_trait]
impl Verifier for Sha256SumsTls {
    async fn verify(
        &self,
        _file: &Path,
        file_sha256_hex: &str,
        http: &reqwest::Client,
    ) -> Result<()> {
        let expected =
            fetch_expected_hash(http, &self.sums_url, &self.filename, SumsStyle::Gnu).await?;
        if file_sha256_hex != expected {
            anyhow::bail!(
                "sha256 mismatch\n  expected: {expected} (from {})\n  actual:   {file_sha256_hex}",
                self.sums_url
            );
        }
        eprintln!("Checksum OK: {expected}");
        Ok(())
    }

    fn checksum_source(&self) -> ChecksumSource {
        ChecksumSource::Document {
            url: self.sums_url.clone(),
            filename: self.filename.clone(),
            style: SumsStyle::Gnu,
            algorithm: HashAlgorithm::Sha256,
        }
    }
}

pub struct Sha512SumsTls {
    pub sums_url: Url,
    pub filename: String,
}

impl Sha512SumsTls {
    pub fn new(sums_url: Url, filename: String) -> Self {
        Self { sums_url, filename }
    }
}

#[async_trait]
impl Verifier for Sha512SumsTls {
    async fn verify(
        &self,
        file: &Path,
        _file_sha256_hex: &str,
        http: &reqwest::Client,
    ) -> Result<()> {
        let expected =
            fetch_expected_hash(http, &self.sums_url, &self.filename, SumsStyle::Gnu).await?;
        let actual = sha512_file(file).await?;
        if actual != expected {
            anyhow::bail!(
                "sha512 mismatch\n  expected: {expected} (from {})\n  actual:   {actual}",
                self.sums_url
            );
        }
        eprintln!("Checksum OK (sha512): {expected}");
        Ok(())
    }

    fn checksum_source(&self) -> ChecksumSource {
        ChecksumSource::Document {
            url: self.sums_url.clone(),
            filename: self.filename.clone(),
            style: SumsStyle::Gnu,
            algorithm: HashAlgorithm::Sha512,
        }
    }
}

/// FreeBSD-style `CHECKSUM.SHA256` file — BSD-traditional format
/// `SHA256 (filename) = hex` rather than the Linux `<hex>  filename`
/// format the other SUMS verifiers handle. Same threat model.
pub struct Sha256BsdSumsTls {
    pub sums_url: Url,
    pub filename: String,
}

impl Sha256BsdSumsTls {
    pub fn new(sums_url: Url, filename: String) -> Self {
        Self { sums_url, filename }
    }
}

#[async_trait]
impl Verifier for Sha256BsdSumsTls {
    async fn verify(
        &self,
        _file: &Path,
        file_sha256_hex: &str,
        http: &reqwest::Client,
    ) -> Result<()> {
        let expected =
            fetch_expected_hash(http, &self.sums_url, &self.filename, SumsStyle::Bsd).await?;
        if file_sha256_hex != expected {
            anyhow::bail!(
                "sha256 mismatch\n  expected: {expected} (from {})\n  actual:   {file_sha256_hex}",
                self.sums_url
            );
        }
        eprintln!("Checksum OK: {expected}");
        Ok(())
    }

    fn checksum_source(&self) -> ChecksumSource {
        ChecksumSource::Document {
            url: self.sums_url.clone(),
            filename: self.filename.clone(),
            style: SumsStyle::Bsd,
            algorithm: HashAlgorithm::Sha256,
        }
    }
}

/// Parse a BSD-style `CHECKSUM.SHA256` listing. Each non-empty,
/// non-comment line is `SHA256 (filename) = hex`. Whitespace is
/// flexible. Mixed formats (some lines BSD, some Linux) are not
/// supported, but vendors don't mix.
pub(super) fn parse_bsd_sums_file(body: &str, filename: &str) -> Option<String> {
    let needle_open = format!("({filename})");
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(open) = line.find(&needle_open) else {
            continue;
        };
        // After `(filename)` look for `= hex`.
        let after = &line[open + needle_open.len()..];
        let after = after.trim_start();
        let Some(rest) = after.strip_prefix('=') else {
            continue;
        };
        let hash = rest.trim();
        if !hash.is_empty() {
            return Some(hash.to_string());
        }
    }
    None
}

/// Verifier of last resort for vendors that don't publish a
/// machine-readable hash for the specific binary they serve. It
/// trusts whatever the TLS connection delivered and logs a line so
/// the operator knows there's no per-image hash check happening.
/// Used by the Talos factory, which builds nocloud images
/// dynamically from a content-addressed schematic and does not
/// expose a sidecar `.sha256` for the resulting binary.
pub struct TlsTrustOnly {
    pub note: String,
}

#[async_trait]
impl Verifier for TlsTrustOnly {
    async fn verify(
        &self,
        _file: &Path,
        _file_sha256_hex: &str,
        _http: &reqwest::Client,
    ) -> Result<()> {
        eprintln!("Trust: TLS only — {}", self.note);
        Ok(())
    }

    fn checksum_source(&self) -> ChecksumSource {
        ChecksumSource::None {
            note: self.note.clone(),
        }
    }
}

/// Some vendors (Alpine) publish a per-image sidecar URL that is just
/// the bare hash on a single line — no filename, no comment. Different
/// shape from a `<HASH>SUMS` file but same threat model.
pub struct Sha512SidecarTls {
    pub sidecar_url: Url,
}

#[async_trait]
impl Verifier for Sha512SidecarTls {
    async fn verify(
        &self,
        file: &Path,
        _file_sha256_hex: &str,
        http: &reqwest::Client,
    ) -> Result<()> {
        let expected = fetch_expected_hash(http, &self.sidecar_url, "", SumsStyle::Bare).await?;
        let actual = sha512_file(file).await?;
        if actual != expected {
            anyhow::bail!(
                "sha512 mismatch\n  expected: {expected} (from {})\n  actual:   {actual}",
                self.sidecar_url
            );
        }
        eprintln!("Checksum OK (sha512): {expected}");
        Ok(())
    }

    fn checksum_source(&self) -> ChecksumSource {
        ChecksumSource::Document {
            url: self.sidecar_url.clone(),
            filename: String::new(),
            style: SumsStyle::Bare,
            algorithm: HashAlgorithm::Sha512,
        }
    }
}

/// Layout of a vendor checksum document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SumsStyle {
    /// `<hex>  [*]<filename>` per line (GNU coreutils, Linux style).
    Gnu,
    /// `SHA256 (<filename>) = <hex>` per line (BSD style).
    Bsd,
    /// The document holds only the hash (a per-image sidecar file).
    Bare,
    /// A vendor document with no generic checksum layout (a JSON feed,
    /// an HTML page); only vendor-specific code can read the hash.
    VendorDocument,
}

/// Fetch a vendor checksum document and return the hash it gives for
/// `filename` (ignored for [`SumsStyle::Bare`]).
pub async fn fetch_expected_hash(
    http: &reqwest::Client,
    url: &Url,
    filename: &str,
    style: SumsStyle,
) -> Result<String> {
    eprintln!("Fetching {url}");
    let body = http
        .get(url.clone())
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("status from {url}"))?
        .text()
        .await
        .with_context(|| format!("read body of {url}"))?;
    parse_expected_hash(&body, filename, style).ok_or_else(|| match style {
        SumsStyle::Bare => anyhow::anyhow!("empty checksum sidecar at {url}"),
        SumsStyle::VendorDocument => {
            anyhow::anyhow!("{url} has no generic checksum layout to read")
        }
        SumsStyle::Gnu | SumsStyle::Bsd => {
            anyhow::anyhow!("filename {filename} not found in {url}")
        }
    })
}

/// Find the hash for `filename` in a checksum document of the given
/// style. For [`SumsStyle::Bare`] the first whitespace-separated token
/// is the hash and `filename` is not consulted.
pub fn parse_expected_hash(body: &str, filename: &str, style: SumsStyle) -> Option<String> {
    match style {
        SumsStyle::Gnu => parse_sums_file(body, filename),
        SumsStyle::Bsd => parse_bsd_sums_file(body, filename),
        SumsStyle::Bare => body.split_whitespace().next().map(str::to_string),
        SumsStyle::VendorDocument => None,
    }
}

/// Parse a `<HASH>SUMS`-style listing. Each non-empty, non-comment line
/// is `<hex>  [*]<filename>`. The asterisk prefix means binary mode and
/// is stripped; whitespace-as-separator handles single-space or
/// multi-space delimiters. The hash function isn't validated here —
/// callers tell upstream which hash they're expecting via the URL.
pub(super) fn parse_sums_file(body: &str, filename: &str) -> Option<String> {
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.splitn(2, char::is_whitespace);
        let hash = parts.next()?.trim();
        let rest = parts.next()?.trim().trim_start_matches('*');
        if rest == filename {
            return Some(hash.to_string());
        }
    }
    None
}

pub async fn sha256_file(file: &Path) -> Result<String> {
    hash_file::<Sha256>(file).await
}

pub async fn sha512_file(file: &Path) -> Result<String> {
    hash_file::<Sha512>(file).await
}

async fn hash_file<H: Digest>(file: &Path) -> Result<String> {
    let mut f = tokio::fs::File::open(file)
        .await
        .with_context(|| format!("open {}", file.display()))?;
    let mut hasher = H::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format_hex(&hasher.finalize()))
}

fn format_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{:02x}", b);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sums_file_canonical_lines() {
        let body = "\
abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234 *foo.img\n\
deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef  bar.img\n\
# comment line\n\
\n\
0000000000000000000000000000000000000000000000000000000000000000  baz.img\n";
        assert_eq!(
            parse_sums_file(body, "foo.img"),
            Some("abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234".to_string())
        );
        assert_eq!(
            parse_sums_file(body, "bar.img"),
            Some("deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string())
        );
        assert_eq!(parse_sums_file(body, "missing.img"), None);
    }

    #[test]
    fn parse_bsd_sums_file_canonical_lines() {
        let body = "\
SHA256 (FreeBSD-15.0-RELEASE-amd64-BASIC-CLOUDINIT-zfs.raw.xz) = 311661446d4654a81a687afd6cbca72cf32848f5251f072a7d4067c42e173324
SHA256 (FreeBSD-15.0-RELEASE-amd64-BASIC-CLOUDINIT-ufs.raw.xz) = aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
# comment line
";
        assert_eq!(
            parse_bsd_sums_file(
                body,
                "FreeBSD-15.0-RELEASE-amd64-BASIC-CLOUDINIT-zfs.raw.xz"
            ),
            Some("311661446d4654a81a687afd6cbca72cf32848f5251f072a7d4067c42e173324".to_string())
        );
        assert_eq!(
            parse_bsd_sums_file(
                body,
                "FreeBSD-15.0-RELEASE-amd64-BASIC-CLOUDINIT-ufs.raw.xz"
            ),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string())
        );
        assert_eq!(parse_bsd_sums_file(body, "missing.raw.xz"), None);
    }

    #[test]
    fn parse_sums_file_works_for_sha512_lines() {
        // Hash length isn't validated; whatever hex appears in the
        // first column is returned as-is. SHA-512 produces 128 hex
        // chars vs SHA-256's 64.
        let body = "00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000  debian-13-genericcloud-amd64.qcow2\n";
        assert_eq!(
            parse_sums_file(body, "debian-13-genericcloud-amd64.qcow2"),
            Some("00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000".to_string())
        );
    }

    #[test]
    fn parse_expected_hash_dispatches_by_style() {
        assert_eq!(
            parse_expected_hash("abc123  *img.qcow2\n", "img.qcow2", SumsStyle::Gnu),
            Some("abc123".to_string())
        );
        assert_eq!(
            parse_expected_hash("SHA256 (img.qcow2) = abc123\n", "img.qcow2", SumsStyle::Bsd),
            Some("abc123".to_string())
        );
        assert_eq!(
            parse_expected_hash("abc123\n", "ignored", SumsStyle::Bare),
            Some("abc123".to_string())
        );
        assert_eq!(
            parse_expected_hash("  \n", "ignored", SumsStyle::Bare),
            None
        );
    }

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap_or_else(|e| panic!("bad test url {s}: {e}"))
    }

    #[test]
    fn checksum_source_describes_each_verifier() {
        assert_eq!(
            Sha256Pinned::new("abc".to_string()).checksum_source(),
            ChecksumSource::Pinned {
                algorithm: HashAlgorithm::Sha256,
                hash: "abc".to_string(),
                document: None,
            }
        );
        assert_eq!(
            Sha256SumsTls::new(url("https://v.example/SHA256SUMS"), "img".to_string())
                .checksum_source(),
            ChecksumSource::Document {
                url: url("https://v.example/SHA256SUMS"),
                filename: "img".to_string(),
                style: SumsStyle::Gnu,
                algorithm: HashAlgorithm::Sha256,
            }
        );
        assert_eq!(
            Sha512SumsTls::new(url("https://v.example/SHA512SUMS"), "img".to_string())
                .checksum_source(),
            ChecksumSource::Document {
                url: url("https://v.example/SHA512SUMS"),
                filename: "img".to_string(),
                style: SumsStyle::Gnu,
                algorithm: HashAlgorithm::Sha512,
            }
        );
        assert_eq!(
            Sha256BsdSumsTls::new(url("https://v.example/CHECKSUM.SHA256"), "img".to_string())
                .checksum_source(),
            ChecksumSource::Document {
                url: url("https://v.example/CHECKSUM.SHA256"),
                filename: "img".to_string(),
                style: SumsStyle::Bsd,
                algorithm: HashAlgorithm::Sha256,
            }
        );
        assert_eq!(
            Sha512SidecarTls {
                sidecar_url: url("https://v.example/img.sha512")
            }
            .checksum_source(),
            ChecksumSource::Document {
                url: url("https://v.example/img.sha512"),
                filename: String::new(),
                style: SumsStyle::Bare,
                algorithm: HashAlgorithm::Sha512,
            }
        );
        assert_eq!(
            TlsTrustOnly {
                note: "no hash".to_string()
            }
            .checksum_source(),
            ChecksumSource::None {
                note: "no hash".to_string()
            }
        );
    }
}
