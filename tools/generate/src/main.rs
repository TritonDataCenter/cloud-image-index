// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Query vendors with the resolvers and write the index tree.
//!
//! The commands map onto a CI matrix with one job per vendor:
//!
//! ```text
//! cloud-image-index-generate vendors [--json]
//!     list the vendor ids (the matrix)
//! cloud-image-index-generate vendor <name> --previous <site> --out <fragment>
//!     one vendor's job: write that vendor alone as a small, valid tree
//! cloud-image-index-generate assemble --site <site> <fragment>...
//!     final job: replace each fragment's vendor in <site>, keep the rest,
//!     rebuild v1/index.json and check the whole tree
//! cloud-image-index-generate all --site <site> [--vendor <name>]...
//!     every vendor in one process (local convenience)
//! ```
//!
//! Known builds keep their `first_seen` from the previous tree. A vendor
//! that fails writes no fragment, so assembly keeps its previous files.
//! A previous tree that exists but cannot be read stops every command
//! before anything is written.
//! A release whose image the vendor no longer serves (HTTP 404 or 410)
//! is reported and left out; any other failure fails the vendor (see
//! "Update policy" in docs/design.md). A command exits non-zero if its
//! vendor failed or the tree it wrote does not validate.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use api::{Build, BuildList, Distro, OsFamily, Release};
use clap::{Parser, Subcommand};
use generate::lifecycle;
use generate::policy::{self, Candidate};
use generate::{
    HeadInfo, OnResolveFailure, artifact, build, digests, fail_if_transient, fill_from_previous,
    http_failure, is_transient_error, on_resolve_failure, signature, vendor_does_not_serve,
    with_retries, write,
};
use resolvers::verify::{ChecksumSource, fetch_expected_hash};
use resolvers::{ResolvedImage, Vendor, VersionEntry};

/// A vendor whose run fails transiently (no response, 429, 5xx) is tried
/// this many times in all, this far apart, before it counts as failed.
const VENDOR_ATTEMPTS: usize = 3;
const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(20);

/// Bounds on every request. reqwest has none by default, so a vendor
/// server that accepts a connection and then stalls would hold its job
/// until the CI limit. The generator only fetches listings, checksum
/// files and metadata, and HEADs images, so two minutes is generous.
/// A timeout is a transient failure: the vendor is retried, then keeps
/// its previous files.
///
/// The connect timeout is shared evenly among a host's addresses, and a
/// mirror with one dead host costs that share on every connection that
/// tries it first (connections are not reused; see `http_client_with`).
/// `cloud.debian.org` has two hosts, one of which accepted no
/// connections on 2026-10-07: at 30 s each such connection lost 15 s,
/// making Debian's job about 75 s. A healthy host connects in well
/// under a second, so 10 s (5 s per address for two) still leaves
/// plenty of room.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

const USER_AGENT: &str =
    "cloud-image-index (+https://github.com/TritonDataCenter/cloud-image-index)";

#[derive(Parser)]
#[command(about = "Query vendors and write the cloud-image-index file tree")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List the vendor ids, one per line (or a JSON array with --json).
    Vendors {
        #[arg(long)]
        json: bool,
    },
    /// Print the OpenAPI document this version publishes as
    /// `v1/openapi.json`. Run after changing the API to refresh the
    /// copy in `examples/`.
    Openapi,
    /// Generate one vendor and write it as a fragment: a tree holding
    /// only that vendor. Writes nothing if the vendor fails.
    Vendor {
        #[arg(value_enum)]
        vendor: Vendor,
        /// The published tree, for keeping known builds' first_seen.
        #[arg(long)]
        previous: PathBuf,
        /// Directory to write the fragment's `v1/` into.
        #[arg(long)]
        out: PathBuf,
    },
    /// Merge vendor fragments into the published tree, keep vendors
    /// without a fragment as they were, rebuild v1/index.json and check
    /// the result.
    Assemble {
        /// The published tree to update in place.
        #[arg(long)]
        site: PathBuf,
        /// Fragment directories written by `vendor`.
        fragments: Vec<PathBuf>,
    },
    /// Generate vendors in one process and update the tree in place.
    All {
        #[arg(long)]
        site: PathBuf,
        /// Only these vendors (default: all built-in vendors).
        #[arg(long, value_enum)]
        vendor: Vec<Vendor>,
    },
}

fn os_family(vendor: Vendor) -> OsFamily {
    match vendor.os() {
        "bsd" => OsFamily::Bsd,
        "illumos" => OsFamily::Illumos,
        _ => OsFamily::Linux,
    }
}

async fn fetch_head(http: &reqwest::Client, url: &url::Url) -> Result<HeadInfo> {
    let resp = http
        .head(url.clone())
        .send()
        .await
        .with_context(|| format!("HEAD {url}"))?
        .error_for_status()
        .with_context(|| format!("HEAD {url}"))?;
    let header = |name: reqwest::header::HeaderName| {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let redirects_off_host = resp.url().host_str() != url.host_str();
    Ok(HeadInfo {
        size: header(reqwest::header::CONTENT_LENGTH).and_then(|v| v.parse().ok()),
        last_modified: header(reqwest::header::LAST_MODIFIED),
        etag: header(reqwest::header::ETAG),
        redirects_off_host,
    })
}

/// Counts of what went wrong, for the summary and exit status.
#[derive(Default)]
struct Problems {
    failed_vendors: Vec<String>,
    warnings: usize,
}

impl Problems {
    fn warn(&mut self, msg: String) {
        eprintln!("warning: {msg}");
        self.warnings += 1;
    }
}

/// The build for a resolved image; `Ok(None)` when the vendor
/// definitely does not serve the image (404/410). Metadata this run
/// could not read is taken from `previous` for the same URL.
async fn build_for(
    http: &reqwest::Client,
    vendor: Vendor,
    image: &ResolvedImage,
    previous: Option<&api::tree::Tree>,
    now: chrono::DateTime<chrono::Utc>,
    problems: &mut Problems,
) -> Result<Option<Build>> {
    let mut head_failed = false;
    let mut checksum_failed = false;
    let head = match fetch_head(http, &image.url).await {
        Ok(h) => h,
        Err(e) => {
            let e = fail_if_transient(vendor, e)?;
            if http_failure(&e)
                .flatten()
                .is_some_and(vendor_does_not_serve)
            {
                problems.warn(format!(
                    "{vendor}: left out, vendor does not serve it: {e:#}"
                ));
                return Ok(None);
            }
            problems.warn(format!("{vendor}: {e:#}"));
            head_failed = true;
            HeadInfo::default()
        }
    };
    let source = image.verifier.checksum_source();
    let fetched = match &source {
        ChecksumSource::Document {
            url,
            filename,
            style,
            ..
        } => match fetch_expected_hash(http, url, filename, *style).await {
            Ok(hash) => Some(hash),
            Err(e) => {
                let e = fail_if_transient(vendor, e)?;
                problems.warn(format!("{vendor}: checksum for {}: {e:#}", image.url));
                checksum_failed = true;
                None
            }
        },
        ChecksumSource::Pinned { .. } | ChecksumSource::None { .. } => None,
    };
    // Publish only the signatures the vendor actually serves.
    let mut signatures_failed = false;
    let mut served_signatures = Vec::new();
    for sig in &image.facts.signatures {
        match fetch_head(http, &sig.url).await {
            Ok(_) => served_signatures.push(signature(sig)),
            Err(e) => {
                let e = fail_if_transient(vendor, e)?;
                if !http_failure(&e)
                    .flatten()
                    .is_some_and(vendor_does_not_serve)
                {
                    problems.warn(format!("{vendor}: signature {}: {e:#}", sig.url));
                    signatures_failed = true;
                }
            }
        }
    }
    let mut artifact = artifact(vendor, image, &head, digests(&source, fetched));
    if let Some(integrity) = artifact.integrity.as_mut() {
        integrity.signatures = served_signatures;
    }
    fill_from_previous(
        &mut artifact,
        previous.and_then(|p| write::previous_artifact(p, &vendor.id(), image.url.as_str())),
        head_failed,
        checksum_failed,
        signatures_failed,
    );
    Ok(Some(build(image, artifact, now)))
}

/// Fetch endoflife.date's cycles for one product.
async fn fetch_cycles(http: &reqwest::Client, product: &str) -> Result<Vec<lifecycle::Cycle>> {
    let url = lifecycle::product_url(product);
    let body = http
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("status from {url}"))?
        .text()
        .await
        .with_context(|| format!("read body of {url}"))?;
    lifecycle::parse_product(&body).with_context(|| format!("parse {url}"))
}

/// Prune releases endoflife.date says have ended from `entries` (see
/// [`lifecycle`]).
///
/// What happens when endoflife.date cannot be read: the vendor fails,
/// so it keeps its previous files (after the usual retries, since an
/// outage is a transient failure). That never re-offers a release a
/// previous run pruned, at the cost of freezing every covered vendor
/// while endoflife.date is down. To publish without pruning instead,
/// replace the `Err(e) => return Err(..)` arm below with a warning and
/// `return Ok(())`: vendors then update, and releases that have ended
/// but that the vendor still lists may briefly come back.
async fn apply_lifecycle(
    http: &reqwest::Client,
    vendor: Vendor,
    entries: &mut [VersionEntry],
    now: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    let Some(product) = lifecycle::product(vendor) else {
        return Ok(());
    };
    let cycles = match fetch_cycles(http, product).await {
        Ok(cycles) => cycles,
        Err(e) => {
            return Err(e.context(format!(
                "{vendor}: cannot read endoflife.date to prune ended releases, keeping \
                 previous files (to publish without pruning instead, see apply_lifecycle \
                 in tools/generate/src/main.rs)"
            )));
        }
    };
    for note in lifecycle::prune(vendor, entries, &cycles, now.date_naive()) {
        eprintln!("note: {note}");
    }
    Ok(())
}

async fn generate_vendor(
    http: &reqwest::Client,
    vendor: Vendor,
    previous: Option<&api::tree::Tree>,
    now: chrono::DateTime<chrono::Utc>,
    problems: &mut Problems,
) -> Result<(Distro, Vec<BuildList>)> {
    let profile = resolvers::lookup(vendor);
    let mut entries: Vec<VersionEntry> = profile
        .list_versions(http)
        .await
        .with_context(|| format!("{vendor}: list versions"))?;
    apply_lifecycle(http, vendor, &mut entries, now).await?;

    // Resolve every included entry and check the vendor serves its
    // image before planning, so aliases only land on releases that make
    // it into the index.
    let mut resolved: Vec<(VersionEntry, ResolvedImage, Build)> = Vec::new();
    for entry in entries {
        if !policy::include_entry(&entry).with_context(|| format!("{vendor}: list versions"))? {
            continue;
        }
        let image = match profile.resolve(&entry.token, http).await {
            Ok(image) => image,
            Err(e) => match on_resolve_failure(http_failure(&e)) {
                OnResolveFailure::DropRelease => {
                    problems.warn(format!(
                        "{vendor}: left out {:?}, vendor does not serve it: {e:#}",
                        entry.token
                    ));
                    continue;
                }
                OnResolveFailure::FailVendor => {
                    return Err(e.context(format!(
                        "{vendor}: resolve {:?} failed, keeping previous files",
                        entry.token
                    )));
                }
            },
        };
        if let Some(build) = build_for(http, vendor, &image, previous, now, problems).await? {
            resolved.push((entry, image, build));
        }
    }
    let homepage = resolved
        .first()
        .map(|(_, image, _)| image.homepage.to_string())
        .with_context(|| format!("{vendor}: no release has an image the vendor serves"))?;

    let candidates: Vec<Candidate<'_>> = resolved
        .iter()
        .map(|(entry, image, _)| Candidate { entry, image })
        .collect();
    let mut releases = Vec::new();
    let mut build_lists = Vec::new();
    let plan = policy::plan(vendor.label(), &candidates)
        .with_context(|| format!("{vendor}: cannot place every release, keeping previous files"))?;
    for planned in plan.releases {
        let mut list = BuildList {
            distro: vendor.id(),
            release: planned.id.clone(),
            builds: vec![resolved[planned.candidate].2.clone()],
        };
        write::preserve_first_seen(
            previous.and_then(|p| write::previous_builds(p, &vendor.id(), &planned.id)),
            &mut list,
        );
        releases.push(Release {
            id: planned.id,
            version: Some(planned.version),
            title: Some(planned.title),
            aliases: planned.aliases,
            eol_date: planned.eol_date,
            osinfo: None,
        });
        build_lists.push(list);
    }

    if releases.is_empty() {
        anyhow::bail!("{vendor}: no release has an image the vendor serves");
    }
    let distro = Distro {
        id: vendor.id(),
        name: Some(vendor.label().to_string()),
        os_family: os_family(vendor),
        homepage: Some(homepage),
        dev_channel: plan.dev_channel,
        releases,
    };
    Ok((distro, build_lists))
}

/// [`generate_vendor`], retried on transient failure. Returns the
/// distro and the number of warnings from the attempt that succeeded.
async fn generate_vendor_with_retries(
    http: &reqwest::Client,
    vendor: Vendor,
    previous: Option<&api::tree::Tree>,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<((Distro, Vec<BuildList>), usize)> {
    with_retries(
        VENDOR_ATTEMPTS,
        RETRY_DELAY,
        is_transient_error,
        |attempt| async move {
            if attempt > 0 {
                eprintln!(
                    "{vendor}: transient failure, retrying (attempt {} of {VENDOR_ATTEMPTS})",
                    attempt + 1
                );
            }
            let mut problems = Problems::default();
            generate_vendor(http, vendor, previous, now, &mut problems)
                .await
                .map(|d| (d, problems.warnings))
        },
    )
    .await
}

fn http_client() -> Result<reqwest::Client> {
    http_client_with(CONNECT_TIMEOUT, REQUEST_TIMEOUT)
}

fn http_client_with(
    connect: std::time::Duration,
    request: std::time::Duration,
) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(connect)
        .timeout(request)
        // Don't keep idle connections for reuse. Vendor servers close
        // idle keep-alive connections, and reusing one fails with
        // "connection closed before message completed": the Fedora job
        // failed 5 of 24 times with pooling and 0 of 24 without
        // (2026-10-06). A daily job can afford the extra handshakes.
        .pool_max_idle_per_host(0)
        .build()
        .context("build HTTP client")
}

/// Write `distros` as the tree at `root` and check it.
fn write_and_validate(root: &Path, distros: &[(Distro, Vec<BuildList>)]) -> Result<()> {
    write::write_tree(root, &write::distro_list(distros), distros)?;
    api::tree::validate(root).map_err(|p| {
        anyhow::anyhow!(
            "{} does not validate:\n  {}",
            root.display(),
            p.join("\n  ")
        )
    })?;
    Ok(())
}

fn vendor_order() -> Vec<String> {
    Vendor::ALL.iter().map(Vendor::id).collect()
}

fn summary(distros: &[(Distro, Vec<BuildList>)]) -> String {
    let releases: usize = distros.iter().map(|(d, _)| d.releases.len()).sum();
    format!("{} distros, {releases} releases", distros.len())
}

async fn cmd_vendor(vendor: Vendor, previous: &Path, out: &Path) -> Result<bool> {
    let http = http_client()?;
    let previous = write::load_previous(previous)?;
    if out.exists() {
        std::fs::remove_dir_all(out).with_context(|| format!("remove {}", out.display()))?;
    }
    eprintln!("== {vendor}");
    let generated =
        generate_vendor_with_retries(&http, vendor, previous.as_ref(), chrono::Utc::now()).await;
    match generated {
        Ok((d, warnings)) => {
            let fragment = vec![d];
            write_and_validate(out, &fragment)?;
            eprintln!(
                "{vendor}: wrote {} to {}; {warnings} warnings",
                summary(&fragment),
                out.display(),
            );
            Ok(true)
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            eprintln!("{vendor}: no fragment written; its previous files will be kept");
            Ok(false)
        }
    }
}

fn cmd_assemble(site: &Path, fragments: &[PathBuf]) -> Result<bool> {
    let previous = write::load_previous(site)?;
    let mut fresh = Vec::new();
    let mut ok = true;
    for dir in fragments {
        if !dir.join("v1").exists() {
            eprintln!("{}: no fragment (vendor failed); skipped", dir.display());
            continue;
        }
        match write::read_fragment(dir) {
            Ok(mut f) => fresh.append(&mut f),
            Err(e) => {
                eprintln!("error: {e:#}");
                ok = false;
            }
        }
    }
    let updated: Vec<String> = fresh.iter().map(|(d, _)| d.id.clone()).collect();
    let merged = write::merge(&vendor_order(), previous.as_ref(), fresh)?;
    write_and_validate(site, &merged)?;
    eprintln!(
        "assembled {} in {}; updated: {updated:?}",
        summary(&merged),
        site.display()
    );
    Ok(ok)
}

async fn cmd_all(site: &Path, only: &[Vendor]) -> Result<bool> {
    let http = http_client()?;
    let previous = write::load_previous(site)?;
    let now = chrono::Utc::now();
    let mut problems = Problems::default();
    let mut fresh = Vec::new();
    for vendor in Vendor::ALL {
        if !only.is_empty() && !only.contains(&vendor) {
            continue;
        }
        eprintln!("== {vendor}");
        match generate_vendor_with_retries(&http, vendor, previous.as_ref(), now).await {
            Ok((d, warnings)) => {
                problems.warnings += warnings;
                fresh.push(d);
            }
            Err(e) => {
                eprintln!("error: {e:#}");
                problems.failed_vendors.push(vendor.id());
            }
        }
    }
    let merged = write::merge(&vendor_order(), previous.as_ref(), fresh)?;
    write_and_validate(site, &merged)?;
    eprintln!(
        "wrote {} to {}; {} warnings; failed vendors: {:?}",
        summary(&merged),
        site.display(),
        problems.warnings,
        problems.failed_vendors
    );
    Ok(problems.failed_vendors.is_empty())
}

async fn run(args: Args) -> Result<bool> {
    match args.command {
        Command::Vendors { json } => {
            let ids = vendor_order();
            if json {
                println!("{}", serde_json::to_string(&ids)?);
            } else {
                for id in ids {
                    println!("{id}");
                }
            }
            Ok(true)
        }
        Command::Openapi => {
            let spec = api::openapi().map_err(anyhow::Error::msg)?;
            println!("{}", serde_json::to_string_pretty(&spec)?);
            Ok(true)
        }
        Command::Vendor {
            vendor,
            previous,
            out,
        } => cmd_vendor(vendor, &previous, &out).await,
        Command::Assemble { site, fragments } => cmd_assemble(&site, &fragments),
        Command::All { site, vendor } => cmd_all(&site, &vendor).await,
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Args::parse()).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[tokio::test]
    async fn a_server_that_stalls_is_a_transient_failure_not_a_hang() -> Result<()> {
        // Accept the connection and never answer.
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        std::thread::spawn(move || {
            let held: Vec<_> = listener.incoming().take(1).collect();
            std::thread::sleep(Duration::from_secs(10));
            drop(held);
        });

        let http = http_client_with(Duration::from_secs(1), Duration::from_secs(1))?;
        let started = Instant::now();
        let result = http.get(format!("http://{addr}/")).send().await;
        let err = anyhow::Error::from(result.err().context("stalled request succeeded")?);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(is_transient_error(&err), "{err:#}");
        Ok(())
    }
}
