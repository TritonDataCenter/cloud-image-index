// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Turn what the resolvers report into cloud-image-index files.
//!
//! The resolvers were written to resolve one release token to one image
//! for an import tool. This crate maps their output onto the index
//! format as a first approximation: one build (the vendor's current one)
//! and one artifact (the profile's default variant, x86_64) per release.
//! Per-vendor policy that the resolvers do not express (which list
//! entries are releases, which release holds which alias) lives in
//! [`policy`]. Network access is in `main.rs`; everything here is pure
//! so it can be tested without vendors.

pub mod policy;
pub mod write;

use api::{
    Artifact, Build, ChecksumFormat, ChecksumSource as ApiChecksumSource, Compression, Digest,
    DigestAlgorithm, HttpMetadata, ImageFormat, Integrity, Location, LocationKind,
};
use resolvers::verify::{ChecksumSource, HashAlgorithm, SumsStyle};
use resolvers::{ResolvedImage, SourceFormat, Vendor};

/// What a HEAD request for an image URL reported.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HeadInfo {
    pub size: Option<u64>,
    pub last_modified: Option<String>,
    pub etag: Option<String>,
    /// Whether the URL redirected to a different host.
    pub redirects_off_host: bool,
}

/// Whether an HTTP status is a definite answer that the vendor does
/// not serve the file. Only these leave a build out of the index;
/// other failures (timeouts, 5xx, rate limits) say nothing about the
/// file.
pub fn vendor_does_not_serve(status: u16) -> bool {
    matches!(status, 404 | 410)
}

/// Whether a failed request says nothing about the vendor's files: no
/// HTTP response at all (`None`: connection, TLS, timeout), rate
/// limiting, or a server error. A vendor with a transient failure keeps
/// its previous files rather than publish a partial update.
pub fn is_transient(status: Option<u16>) -> bool {
    match status {
        None => true,
        Some(s) => s == 429 || (500..600).contains(&s),
    }
}

/// Whether `err` is a transient HTTP failure (see [`is_transient`]).
pub fn is_transient_error(err: &anyhow::Error) -> bool {
    http_failure(err).is_some_and(is_transient)
}

/// If `err` is transient, return it as the failure, with context naming
/// `vendor` added on top so the cause stays in the chain (callers, such
/// as the retry, inspect it). Otherwise hand `err` back for the caller to
/// handle.
pub fn fail_if_transient(
    vendor: impl std::fmt::Display,
    err: anyhow::Error,
) -> Result<anyhow::Error, anyhow::Error> {
    if is_transient_error(&err) {
        Err(err.context(format!(
            "{vendor}: transient failure, keeping previous files"
        )))
    } else {
        Ok(err)
    }
}

/// Run `op` up to `attempts` times (at least once), waiting `delay`
/// between attempts, while it fails with an error `retryable` accepts.
/// `op` is given the attempt number, starting at 0. Returns the first
/// success, or the last error.
pub async fn with_retries<T, F, Fut>(
    attempts: usize,
    delay: std::time::Duration,
    retryable: impl Fn(&anyhow::Error) -> bool,
    mut op: F,
) -> anyhow::Result<T>
where
    F: FnMut(usize) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    let mut attempt = 0;
    loop {
        match op(attempt).await {
            Ok(v) => return Ok(v),
            Err(e) if attempt + 1 < attempts && retryable(&e) => {
                attempt += 1;
                tokio::time::sleep(delay).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// If `err` came from an HTTP request, the response status (`None` when
/// there was no response). `None` overall for errors that did not come
/// from HTTP, such as a vendor page the resolver could not parse. A
/// response body that does not decode (e.g. JSON that changed shape)
/// counts as a vendor page that did not parse, not as a missing
/// response, and so does a request that could not be built.
pub fn http_failure(err: &anyhow::Error) -> Option<Option<u16>> {
    err.chain()
        .find_map(|e| e.downcast_ref::<reqwest::Error>())
        .filter(|e| !e.is_decode() && !e.is_builder())
        .map(|e| e.status().map(|s| s.as_u16()))
}

/// What to do when resolving one release fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnResolveFailure {
    /// The vendor said the release is gone (404/410): leave it out.
    DropRelease,
    /// Anything else: fail the vendor, so its previous files are kept and
    /// the failure is visible. This includes errors that did not come
    /// from HTTP, such as a vendor page the resolver could not
    /// understand, which is how a vendor's format change shows up.
    FailVendor,
}

/// Decide what a failed resolve means, from [`http_failure`]'s view of
/// the error.
pub fn on_resolve_failure(failure: Option<Option<u16>>) -> OnResolveFailure {
    match failure {
        Some(Some(status)) if vendor_does_not_serve(status) => OnResolveFailure::DropRelease,
        _ => OnResolveFailure::FailVendor,
    }
}

/// Map a resolver source format to the index's format and compression.
pub fn image_format(format: SourceFormat) -> (ImageFormat, Compression) {
    match format {
        SourceFormat::Qcow2 => (ImageFormat::Qcow2, Compression::None),
        SourceFormat::Xz => (ImageFormat::Raw, Compression::Xz),
        SourceFormat::Raw => (ImageFormat::Raw, Compression::None),
        SourceFormat::Vmdk => (ImageFormat::Vmdk, Compression::None),
        SourceFormat::RawGz => (ImageFormat::Raw, Compression::Gzip),
    }
}

fn digest_algorithm(algorithm: HashAlgorithm) -> DigestAlgorithm {
    match algorithm {
        HashAlgorithm::Sha256 => DigestAlgorithm::Sha256,
        HashAlgorithm::Sha512 => DigestAlgorithm::Sha512,
    }
}

/// Where a hash was read from, in the index's terms.
fn checksum_source(url: &url::Url, filename: &str, style: SumsStyle) -> ApiChecksumSource {
    ApiChecksumSource {
        url: url.to_string(),
        format: match style {
            SumsStyle::Gnu => ChecksumFormat::Gnu,
            SumsStyle::Bsd => ChecksumFormat::Bsd,
            SumsStyle::Bare => ChecksumFormat::Bare,
            SumsStyle::VendorDocument => ChecksumFormat::VendorDocument,
        },
        filename: match style {
            SumsStyle::Bare | SumsStyle::VendorDocument => None,
            SumsStyle::Gnu | SumsStyle::Bsd => Some(filename.to_string()),
        },
    }
}

/// The digests to publish for a verifier's checksum source. `fetched`
/// is the hash read from the vendor's checksum document, for sources
/// that name one; `None` there (the fetch failed) publishes nothing.
pub fn digests(source: &ChecksumSource, fetched: Option<String>) -> Vec<Digest> {
    match source {
        ChecksumSource::Pinned {
            algorithm,
            hash,
            document,
        } => vec![Digest {
            algorithm: digest_algorithm(*algorithm),
            value: hash.to_lowercase(),
            source: document
                .as_ref()
                .map(|d| checksum_source(&d.url, &d.filename, d.style)),
        }],
        ChecksumSource::Document {
            url,
            filename,
            style,
            algorithm,
        } => match fetched {
            Some(value) => vec![Digest {
                algorithm: digest_algorithm(*algorithm),
                value: value.to_lowercase(),
                source: Some(checksum_source(url, filename, *style)),
            }],
            None => Vec::new(),
        },
        ChecksumSource::None { .. } => Vec::new(),
    }
}

/// A vendor signature in the index's terms. The signing key is not yet
/// known for any vendor.
pub fn signature(s: &resolvers::SignatureRef) -> api::Signature {
    api::Signature {
        url: s.url.to_string(),
        kind: match s.kind {
            resolvers::SignatureKind::Detached => api::SignatureKind::PgpDetached,
            resolvers::SignatureKind::Clearsigned => api::SignatureKind::PgpClearsigned,
        },
        signs: s.signs.to_string(),
        key_url: None,
    }
}

/// The build for a resolved image with its one artifact. `now` becomes
/// `first_seen`; the caller keeps an earlier one for known builds.
pub fn build(
    image: &ResolvedImage,
    artifact: Artifact,
    now: chrono::DateTime<chrono::Utc>,
) -> Build {
    Build {
        build: image.version.clone(),
        point_release: image.facts.point_release.clone(),
        published_at: image.facts.published_at,
        first_seen: Some(now),
        osinfo: None,
        artifacts: vec![artifact],
    }
}

/// cloud-init datasources a vendor's images support. SmartOS is not a
/// cloud-init image (it uses `mdata-get`); every other built-in profile
/// resolves a NoCloud-capable image.
pub fn datasources(vendor: Vendor) -> Vec<String> {
    match vendor {
        Vendor::Smartos => Vec::new(),
        _ => vec!["nocloud".to_string()],
    }
}

/// The artifact for a resolved image: the profile's default variant,
/// x86_64 (the only architecture the resolvers handle).
pub fn artifact(
    vendor: Vendor,
    image: &ResolvedImage,
    head: &HeadInfo,
    digests: Vec<Digest>,
) -> Artifact {
    let (format, compression) = image_format(image.format);
    Artifact {
        variant: Some(vendor.default_variant().to_string()),
        default_variant: Some(true),
        arch: "x86_64".to_string(),
        format,
        compression,
        firmware: Vec::new(),
        datasources: datasources(vendor),
        ssh_key_injection: Some(image.ssh_key),
        size: head.size,
        locations: vec![Location {
            url: image.url.to_string(),
            kind: Some(LocationKind::Primary),
            redirects_off_host: Some(head.redirects_off_host),
        }],
        integrity: Some(Integrity {
            digests,
            signatures: Vec::new(),
            // Behind a mirror redirector each request may reach a
            // different mirror, each with its own ETag and sync time, so
            // only metadata from the vendor's own host is published.
            http: Some(if head.redirects_off_host {
                HttpMetadata {
                    last_modified: None,
                    etag: None,
                }
            } else {
                HttpMetadata {
                    last_modified: head.last_modified.clone(),
                    etag: head.etag.clone(),
                }
            }),
        }),
    }
}

/// When this run could not read an artifact's HEAD metadata
/// (`head_failed`), its vendor checksum (`checksum_failed`) or whether
/// its signatures exist (`signatures_failed`), for a reason that does
/// not mean the file is gone, reuse what the previous run published for
/// the same URL, so one failed request does not change the index for a
/// day. Nothing is taken from a different URL.
pub fn fill_from_previous(
    current: &mut Artifact,
    previous: Option<&Artifact>,
    head_failed: bool,
    checksum_failed: bool,
    signatures_failed: bool,
) {
    let url = current.locations.first().map(|l| l.url.clone());
    let Some(previous) = previous.filter(|p| p.locations.first().map(|l| l.url.clone()) == url)
    else {
        return;
    };
    if head_failed {
        current.size = previous.size;
        if let (Some(c), Some(p)) = (current.locations.first_mut(), previous.locations.first()) {
            c.redirects_off_host = p.redirects_off_host;
        }
    }
    let previous = previous.integrity.as_ref();
    let Some(integrity) = current.integrity.as_mut() else {
        return;
    };
    if head_failed {
        integrity.http = previous.and_then(|p| p.http.clone());
    }
    if checksum_failed {
        integrity.digests = previous.map(|p| p.digests.clone()).unwrap_or_default();
    }
    if signatures_failed {
        integrity.signatures = previous.map(|p| p.signatures.clone()).unwrap_or_default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use resolvers::verify::Sha256Pinned;
    use url::Url;

    fn integrity(a: &Artifact) -> &Integrity {
        a.integrity
            .as_ref()
            .unwrap_or_else(|| panic!("artifact has no integrity"))
    }

    fn integrity_mut(a: &mut Artifact) -> &mut Integrity {
        a.integrity
            .as_mut()
            .unwrap_or_else(|| panic!("artifact has no integrity"))
    }

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap_or_else(|e| panic!("bad test url {s}: {e}"))
    }

    #[test]
    fn image_format_covers_every_source_format() {
        assert_eq!(
            image_format(SourceFormat::Qcow2),
            (ImageFormat::Qcow2, Compression::None)
        );
        assert_eq!(
            image_format(SourceFormat::Xz),
            (ImageFormat::Raw, Compression::Xz)
        );
        assert_eq!(
            image_format(SourceFormat::Raw),
            (ImageFormat::Raw, Compression::None)
        );
        assert_eq!(
            image_format(SourceFormat::Vmdk),
            (ImageFormat::Vmdk, Compression::None)
        );
        assert_eq!(
            image_format(SourceFormat::RawGz),
            (ImageFormat::Raw, Compression::Gzip)
        );
    }

    #[test]
    fn pinned_hash_is_published_without_a_source() {
        let d = digests(
            &ChecksumSource::Pinned {
                algorithm: HashAlgorithm::Sha256,
                hash: "ABC".to_string(),
                document: None,
            },
            None,
        );
        assert_eq!(
            d,
            vec![Digest {
                algorithm: DigestAlgorithm::Sha256,
                value: "abc".to_string(),
                source: None
            }]
        );
    }

    #[test]
    fn document_hash_is_published_with_its_source() {
        let source = ChecksumSource::Document {
            url: url("https://v.example/SHA512SUMS"),
            filename: "img.qcow2".to_string(),
            style: SumsStyle::Gnu,
            algorithm: HashAlgorithm::Sha512,
        };
        assert_eq!(
            digests(&source, Some("def".to_string())),
            vec![Digest {
                algorithm: DigestAlgorithm::Sha512,
                value: "def".to_string(),
                source: Some(ApiChecksumSource {
                    url: "https://v.example/SHA512SUMS".to_string(),
                    format: ChecksumFormat::Gnu,
                    filename: Some("img.qcow2".to_string()),
                }),
            }]
        );
        assert_eq!(digests(&source, None), Vec::new());
    }

    #[test]
    fn bare_sidecar_has_no_filename() {
        let source = ChecksumSource::Document {
            url: url("https://v.example/img.sha512"),
            filename: String::new(),
            style: SumsStyle::Bare,
            algorithm: HashAlgorithm::Sha512,
        };
        let d = digests(&source, Some("abc".to_string()));
        assert_eq!(
            d.first()
                .and_then(|d| d.source.as_ref())
                .map(|s| (s.format.clone(), s.filename.clone())),
            Some((ChecksumFormat::Bare, None))
        );
    }

    #[test]
    fn no_vendor_hash_publishes_no_digest() {
        let source = ChecksumSource::None {
            note: "none".to_string(),
        };
        assert_eq!(digests(&source, Some("ignored".to_string())), Vec::new());
    }

    fn resolved(format: SourceFormat, ssh_key: bool) -> ResolvedImage {
        ResolvedImage {
            url: url("https://v.example/img"),
            format,
            os: "linux".to_string(),
            series: "s".to_string(),
            version: "1".to_string(),
            description: String::new(),
            homepage: url("https://v.example/"),
            ssh_key,
            verifier: Box::new(Sha256Pinned::new("abc".to_string())),
            expected_sha256: Some("abc".to_string()),
            facts: Default::default(),
        }
    }

    #[test]
    fn artifact_carries_head_info_and_vendor_facts() {
        let head = HeadInfo {
            size: Some(42),
            last_modified: Some("lm".to_string()),
            etag: Some("\"e\"".to_string()),
            redirects_off_host: false,
        };
        let a = artifact(
            Vendor::Talos,
            &resolved(SourceFormat::Xz, false),
            &head,
            Vec::new(),
        );
        assert_eq!(a.variant.as_deref(), Some("nocloud"));
        assert_eq!(
            (a.format.clone(), a.compression.clone()),
            (ImageFormat::Raw, Compression::Xz)
        );
        assert_eq!(a.size, Some(42));
        assert_eq!(a.ssh_key_injection, Some(false));
        assert_eq!(a.datasources, vec!["nocloud".to_string()]);
        assert_eq!(
            a.locations,
            vec![Location {
                url: "https://v.example/img".to_string(),
                kind: Some(LocationKind::Primary),
                redirects_off_host: Some(false),
            }]
        );
        assert_eq!(
            integrity(&a).http,
            Some(HttpMetadata {
                last_modified: Some("lm".to_string()),
                etag: Some("\"e\"".to_string())
            })
        );
    }

    #[test]
    fn only_404_and_410_mean_the_vendor_does_not_serve_it() {
        assert!(vendor_does_not_serve(404));
        assert!(vendor_does_not_serve(410));
        for status in [200, 301, 403, 429, 500, 503] {
            assert!(!vendor_does_not_serve(status), "{status}");
        }
    }

    #[test]
    fn transient_failures_are_no_response_rate_limits_and_server_errors() {
        assert!(is_transient(None));
        assert!(is_transient(Some(429)));
        assert!(is_transient(Some(500)));
        assert!(is_transient(Some(503)));
        for status in [400, 403, 404, 410] {
            assert!(!is_transient(Some(status)), "{status}");
        }
    }

    #[test]
    fn mirror_etag_and_last_modified_are_not_published() {
        // Each mirror reports its own ETag and sync time for the same
        // file, so publishing them would change the index every run.
        let head = HeadInfo {
            size: Some(42),
            last_modified: Some("lm".to_string()),
            etag: Some("\"e\"".to_string()),
            redirects_off_host: true,
        };
        let a = artifact(
            Vendor::Fedora,
            &resolved(SourceFormat::Qcow2, true),
            &head,
            Vec::new(),
        );
        assert_eq!(a.size, Some(42));
        assert_eq!(
            integrity(&a).http,
            Some(HttpMetadata {
                last_modified: None,
                etag: None
            })
        );
    }

    fn sample_artifact(size: Option<u64>, etag: Option<&str>, digests: Vec<Digest>) -> Artifact {
        let head = HeadInfo {
            size,
            last_modified: None,
            etag: etag.map(str::to_string),
            redirects_off_host: false,
        };
        artifact(
            Vendor::Rocky,
            &resolved(SourceFormat::Qcow2, true),
            &head,
            digests,
        )
    }

    fn sha(v: &str) -> Vec<Digest> {
        vec![Digest {
            algorithm: DigestAlgorithm::Sha256,
            value: v.to_string(),
            source: None,
        }]
    }

    #[test]
    fn failed_head_reuses_previous_size_and_http_metadata() {
        let previous = sample_artifact(Some(42), Some("e"), sha("old"));
        let mut current = sample_artifact(None, None, sha("new"));
        fill_from_previous(&mut current, Some(&previous), true, false, false);
        assert_eq!(current.size, Some(42));
        assert_eq!(integrity(&current).http, integrity(&previous).http);
        // The checksum fetch worked, so this run's digests stand.
        assert_eq!(integrity(&current).digests, sha("new"));
    }

    #[test]
    fn failed_checksum_fetch_reuses_previous_digests() {
        let previous = sample_artifact(Some(42), Some("e"), sha("old"));
        let mut current = sample_artifact(Some(43), Some("f"), Vec::new());
        fill_from_previous(&mut current, Some(&previous), false, true, false);
        assert_eq!(integrity(&current).digests, sha("old"));
        assert_eq!(current.size, Some(43));
    }

    #[test]
    fn nothing_is_reused_from_a_different_url_or_without_failures() {
        let mut previous = sample_artifact(Some(42), Some("e"), sha("old"));
        let mut current = sample_artifact(None, None, Vec::new());
        let untouched = current.clone();
        fill_from_previous(&mut current, Some(&previous), false, false, false);
        assert_eq!(current, untouched);
        previous.locations[0].url = "https://v.example/other".to_string();
        fill_from_previous(&mut current, Some(&previous), true, true, true);
        assert_eq!(current, untouched);
        fill_from_previous(&mut current, None, true, true, true);
        assert_eq!(current, untouched);
    }

    #[test]
    fn pinned_hash_publishes_the_document_it_came_from() {
        let d = digests(
            &ChecksumSource::Pinned {
                algorithm: HashAlgorithm::Sha256,
                hash: "abc".to_string(),
                document: Some(resolvers::verify::ChecksumDocument {
                    url: url("https://v.example/img.CHECKSUM"),
                    filename: "img.qcow2".to_string(),
                    style: SumsStyle::Bsd,
                }),
            },
            None,
        );
        assert_eq!(
            d.first().and_then(|d| d.source.clone()),
            Some(ApiChecksumSource {
                url: "https://v.example/img.CHECKSUM".to_string(),
                format: ChecksumFormat::Bsd,
                filename: Some("img.qcow2".to_string()),
            })
        );
    }

    #[test]
    fn vendor_document_sources_have_no_filename() {
        let d = digests(
            &ChecksumSource::Pinned {
                algorithm: HashAlgorithm::Sha256,
                hash: "abc".to_string(),
                document: Some(resolvers::verify::ChecksumDocument {
                    url: url("https://v.example/releases.json"),
                    filename: String::new(),
                    style: SumsStyle::VendorDocument,
                }),
            },
            None,
        );
        assert_eq!(
            d.first()
                .and_then(|d| d.source.as_ref())
                .map(|s| (s.format.clone(), s.filename.clone())),
            Some((ChecksumFormat::VendorDocument, None))
        );
    }

    #[test]
    fn build_carries_the_vendor_facts() {
        let mut image = resolved(SourceFormat::Qcow2, true);
        image.facts.point_release = Some("9.8".to_string());
        image.facts.published_at = chrono::DateTime::from_timestamp(1_700_000_000, 0);
        let now = chrono::DateTime::from_timestamp(1_800_000_000, 0)
            .unwrap_or_else(|| panic!("bad timestamp"));
        let artifact = sample_artifact(Some(1), None, Vec::new());
        let b = build(&image, artifact.clone(), now);
        assert_eq!(b.build, "1");
        assert_eq!(b.point_release.as_deref(), Some("9.8"));
        assert_eq!(b.published_at, image.facts.published_at);
        assert_eq!(b.first_seen, Some(now));
        assert_eq!(b.artifacts, vec![artifact]);
    }

    #[test]
    fn signature_maps_kind_and_urls() {
        let detached = resolvers::SignatureRef {
            url: url("https://v.example/CHECKSUM.asc"),
            kind: resolvers::SignatureKind::Detached,
            signs: url("https://v.example/CHECKSUM"),
        };
        assert_eq!(
            signature(&detached),
            api::Signature {
                url: "https://v.example/CHECKSUM.asc".to_string(),
                kind: api::SignatureKind::PgpDetached,
                signs: "https://v.example/CHECKSUM".to_string(),
                key_url: None,
            }
        );
        let clear = resolvers::SignatureRef {
            url: url("https://v.example/CHECKSUM"),
            kind: resolvers::SignatureKind::Clearsigned,
            signs: url("https://v.example/CHECKSUM"),
        };
        assert_eq!(signature(&clear).kind, api::SignatureKind::PgpClearsigned);
    }

    #[test]
    fn failed_signature_check_reuses_previous_signatures() {
        let mut previous = sample_artifact(Some(42), None, sha("h"));
        integrity_mut(&mut previous).signatures = vec![api::Signature {
            url: "https://v.example/CHECKSUM.asc".to_string(),
            kind: api::SignatureKind::PgpDetached,
            signs: "https://v.example/CHECKSUM".to_string(),
            key_url: None,
        }];
        let mut current = sample_artifact(Some(42), None, sha("h"));
        fill_from_previous(&mut current, Some(&previous), false, false, true);
        assert_eq!(
            integrity(&current).signatures,
            integrity(&previous).signatures
        );
    }

    #[test]
    fn only_a_vendor_404_drops_a_release_that_failed_to_resolve() {
        assert_eq!(
            on_resolve_failure(Some(Some(404))),
            OnResolveFailure::DropRelease
        );
        assert_eq!(
            on_resolve_failure(Some(Some(410))),
            OnResolveFailure::DropRelease
        );
        // No response, rate limiting, server errors, other statuses, and
        // errors that did not come from HTTP (a page the resolver could
        // not understand) all keep the vendor's previous files.
        for failure in [
            None,
            Some(None),
            Some(Some(429)),
            Some(Some(503)),
            Some(Some(403)),
        ] {
            assert_eq!(
                on_resolve_failure(failure),
                OnResolveFailure::FailVendor,
                "{failure:?}"
            );
        }
    }

    fn flaky(fail_times: usize) -> impl FnMut(usize) -> std::future::Ready<anyhow::Result<usize>> {
        move |attempt| {
            std::future::ready(if attempt < fail_times {
                Err(anyhow::anyhow!("flaky {attempt}"))
            } else {
                Ok(attempt)
            })
        }
    }

    #[tokio::test]
    async fn retries_a_retryable_failure_until_it_succeeds() {
        let r = with_retries(3, std::time::Duration::ZERO, |_| true, flaky(2)).await;
        assert_eq!(r.ok(), Some(2), "succeeds on the third attempt");
    }

    #[tokio::test]
    async fn gives_up_after_the_last_attempt() {
        let r = with_retries(3, std::time::Duration::ZERO, |_| true, flaky(5)).await;
        assert_eq!(r.err().map(|e| e.to_string()), Some("flaky 2".to_string()));
    }

    #[tokio::test]
    async fn does_not_retry_a_failure_that_is_not_retryable() {
        let r = with_retries(3, std::time::Duration::ZERO, |_| false, flaky(1)).await;
        assert_eq!(r.err().map(|e| e.to_string()), Some("flaky 0".to_string()));
    }

    /// A real transient error: nothing listens on the discard port.
    async fn refused_connection() -> anyhow::Error {
        match reqwest::get("http://127.0.0.1:9/").await {
            Ok(r) => anyhow::anyhow!("unexpectedly got a response: {}", r.status()),
            Err(e) => anyhow::Error::new(e),
        }
    }

    #[tokio::test]
    async fn a_transient_failure_stays_recognisable_after_context_is_added() {
        let err = refused_connection().await;
        assert!(is_transient_error(&err), "{err:#}");
        let failed = fail_if_transient("ubuntu", err).map(|e| e.to_string());
        let err = match failed {
            Ok(e) => panic!("transient error not recognised: {e}"),
            Err(e) => e,
        };
        assert!(is_transient_error(&err), "context hid the cause: {err:#}");
        assert!(err.to_string().contains("ubuntu: transient failure"));
    }

    /// A real decode error: a local server answers 200 with a body that
    /// is not JSON, as a vendor that changed its format would.
    async fn undecodable_response() -> anyhow::Error {
        use std::io::{Read, Write};
        let listener = match std::net::TcpListener::bind("127.0.0.1:0") {
            Ok(l) => l,
            Err(e) => return anyhow::Error::new(e),
        };
        let addr = listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_default();
        std::thread::spawn(move || {
            if let Ok((mut conn, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = conn.read(&mut buf);
                let body = "<html>not json</html>";
                let _ = write!(
                    conn,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        let result = async {
            reqwest::get(format!("http://{addr}/"))
                .await?
                .json::<serde_json::Value>()
                .await
        }
        .await;
        match result {
            Ok(v) => anyhow::anyhow!("unexpectedly decoded: {v}"),
            Err(e) => anyhow::Error::new(e),
        }
    }

    #[tokio::test]
    async fn a_response_that_does_not_decode_is_not_transient() {
        // Retrying a vendor whose format changed only delays the failure.
        let err = undecodable_response().await;
        assert!(format!("{err:#}").contains("decoding"), "{err:#}");
        assert!(!is_transient_error(&err), "{err:#}");
        assert_eq!(
            on_resolve_failure(http_failure(&err)),
            OnResolveFailure::FailVendor
        );
    }

    #[test]
    fn a_non_transient_failure_is_handed_back_unchanged() {
        let back = fail_if_transient("ubuntu", anyhow::anyhow!("page did not parse"));
        assert_eq!(
            back.ok().map(|e| e.to_string()),
            Some("page did not parse".to_string())
        );
    }

    #[test]
    fn smartos_lists_no_cloud_init_datasource() {
        assert!(datasources(Vendor::Smartos).is_empty());
        assert_eq!(datasources(Vendor::Ubuntu), vec!["nocloud".to_string()]);
    }
}
