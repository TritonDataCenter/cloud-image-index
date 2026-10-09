// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The SmartOS client that imports images listed in the index into
//! `imgadm`.
//!
//! `import` builds a distro's image with monitor-reef's nocloud-import
//! pipeline, after checking the index's digest against the vendor's own
//! checksum file. `avail` lists the images the index offers, as
//! `imgadm avail` does.

mod import;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

/// The only architecture the pipeline builds images for: bhyve on
/// SmartOS runs x86-64 guests, and the pipeline names its output
/// `*.x86_64.zfs`.
const ARCH: &str = "x86_64";

const DEFAULT_INDEX: &str = "https://tritondatacenter.github.io/cloud-image-index/";

const USER_AGENT: &str = concat!(
    "cloud-image/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/TritonDataCenter/cloud-image-index)"
);

#[derive(Parser)]
#[command(about = "Import images from the cloud-image-index into SmartOS")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List the images the index offers, as `imgadm avail` does.
    ///
    /// Each release's current build, named and versioned as `imgadm list`
    /// will show it after the import.
    ///
    /// The UUID is the one the import will give the image (unknown before
    /// the download when the vendor publishes no sha256). CHECK says how
    /// the import would be checked: `vendor` (the vendor's checksum file
    /// confirms the index), `index` (the index's digest alone) or `none`;
    /// the last two need `import --allow-unverified`.
    Avail(AvailArgs),
    /// Build a distro's image from the index and install it with imgadm.
    ///
    /// The download must match the index's digest and the digest in the
    /// vendor's own checksum file. Off SmartOS this is always a dry run.
    Import(ImportArgs),
}

#[derive(clap::Args)]
struct AvailArgs {
    /// Only this distro's images.
    distro: Option<String>,
    /// Base URL of the index (the directory containing `v1/`).
    #[arg(long, default_value = DEFAULT_INDEX)]
    index: String,
    /// Print JSON instead of a table.
    #[arg(short = 'j', long)]
    json: bool,
    /// Leave out the table header.
    #[arg(short = 'H', long)]
    no_header: bool,
}

#[derive(clap::Args)]
struct ImportArgs {
    /// The distro, as the index names it (e.g. `ubuntu`, `rocky`), or an
    /// image's UUID from `cloud-image avail`.
    distro: String,
    /// A release (e.g. `noble`, `9`) or alias (`latest`, `lts`, `dev`).
    /// Without one, lists the distro's releases and imports nothing.
    release: Option<String>,
    /// Base URL of the index (the directory containing `v1/`).
    #[arg(long, default_value = DEFAULT_INDEX)]
    index: String,
    /// The artifact variant (default: the vendor's default flavour).
    #[arg(long)]
    variant: Option<String>,
    /// Parent dataset for the temporary build zvol (default: `zones` in
    /// the global zone, the delegated dataset in a zone).
    #[arg(long)]
    dataset: Option<String>,
    /// Where to keep the downloaded source (default:
    /// `/var/tmp/cloud-image/cache/<distro>-<release>-<version>`).
    #[arg(long)]
    workdir: Option<PathBuf>,
    /// Where to write the image and manifest (default:
    /// `/var/tmp/cloud-image/image/<distro>-<release>-<version>`).
    #[arg(long)]
    output_dir: Option<PathBuf>,
    /// Build the image files but do not install them.
    #[arg(long)]
    no_install: bool,
    /// Import an image the vendor cannot confirm: one with no published
    /// digest (trusting the TLS connection alone), or one whose digest
    /// the vendor gives only in a document with no generic layout
    /// (checked against the index's digest alone).
    #[arg(long)]
    allow_unverified: bool,
    /// Resolve the image and check the index against the vendor, but
    /// download and build nothing.
    #[arg(long)]
    dry_run: bool,
}

/// Mozilla's root certificates, bundled into the binary.
fn bundled_roots() -> Result<Vec<reqwest::Certificate>> {
    webpki_root_certs::TLS_SERVER_ROOT_CERTS
        .iter()
        .map(|der| reqwest::Certificate::from_der(der).context("bundled root certificate"))
        .collect()
}

fn http_client() -> Result<reqwest::Client> {
    // reqwest is built without a default crypto provider (see the
    // workspace Cargo.toml). An error only means one is already set.
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        // Trust exactly the bundled Mozilla roots. reqwest otherwise asks
        // the system for its CA store, and on SmartOS rustls finds none
        // ("No CA certificates were loaded from the system"). smartos-live's
        // Rust imgadm bundles the same roots for the same reason.
        .tls_certs_only(bundled_roots()?)
        .user_agent(USER_AGENT)
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .context("build HTTP client")
}

/// The index at `base`, read with the generated client.
fn index_client(base: &str) -> Result<client::Client> {
    Ok(client::Client::new_with_client(base, http_client()?))
}

/// The distro `id`, checked against what the index lists before it is
/// fetched: the index's file host answers a missing file with an HTML
/// page, which a client can only report as an unreadable response.
async fn fetch_distro(
    index: &client::Client,
    base: &str,
    id: &str,
) -> Result<client::types::Distro> {
    let distros = index
        .distro_list()
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| format!("read the index at {base}"))?
        .into_inner();
    if !distros.distros.iter().any(|d| d.id == id) {
        let ids: Vec<&str> = distros.distros.iter().map(|d| d.id.as_str()).collect();
        anyhow::bail!("the index has no distro {id:?}; it has: {}", ids.join(", "));
    }
    Ok(index
        .distro(id)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| format!("read {id} from the index at {base}"))?
        .into_inner())
}

/// The current image of every release of `only` (or of every distro),
/// in the index's order. Releases are fetched in parallel. A release with
/// no image this client can build is left out, with a note.
async fn available(
    index: &client::Client,
    base: &str,
    only: Option<&str>,
) -> Result<Vec<import::Available>> {
    let ids: Vec<String> = match only {
        Some(id) => vec![fetch_distro(index, base, id).await?.id],
        None => index
            .distro_list()
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
            .with_context(|| format!("read the index at {base}"))?
            .into_inner()
            .distros
            .into_iter()
            .map(|d| d.id)
            .collect(),
    };
    let mut distros = tokio::task::JoinSet::new();
    for (n, id) in ids.into_iter().enumerate() {
        let index = index.clone();
        distros.spawn(async move {
            let distro = index.distro(&id).await;
            (n, id, distro)
        });
    }
    let mut releases = tokio::task::JoinSet::new();
    while let Some(joined) = distros.join_next().await {
        let (n, id, distro) = joined.context("fetch a distro")?;
        let distro = distro
            .map_err(|e| anyhow::anyhow!("{e}"))
            .with_context(|| format!("read {id} from the index at {base}"))?
            .into_inner();
        let distro = std::sync::Arc::new(distro);
        for (m, release) in distro.releases.iter().enumerate() {
            let index = index.clone();
            let distro = distro.clone();
            let release = release.id.clone();
            releases.spawn(async move {
                let builds = index.release_builds(&distro.id, &release).await;
                ((n, m), distro, release, builds)
            });
        }
    }
    let mut rows = Vec::new();
    while let Some(joined) = releases.join_next().await {
        let (order, distro, release, builds) = joined.context("fetch a release")?;
        let builds = builds
            .map_err(|e| anyhow::anyhow!("{e}"))
            .with_context(|| format!("read {} {release} from the index at {base}", distro.id))?
            .into_inner();
        match import::available(&distro, &builds) {
            Ok(row) => rows.push((order, row)),
            Err(e) => eprintln!("note: leaving out {} {release}: {e:#}", distro.id),
        }
    }
    rows.sort_by_key(|(order, _)| *order);
    Ok(rows.into_iter().map(|(_, row)| row).collect())
}

async fn avail_cmd(args: AvailArgs) -> Result<()> {
    let base = args.index.trim_end_matches('/');
    let index = index_client(base)?;
    let rows = available(&index, base, args.distro.as_deref()).await?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&import::avail_json(&rows))?
        );
    } else {
        print!("{}", import::avail_table(&rows, !args.no_header));
    }
    Ok(())
}

/// Create `dir` and its parents. When a file stands where a directory
/// must go (say the binary itself, saved as /var/tmp/cloud-image), the
/// error names it.
fn create_dir(dir: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(|e| {
        let blocker = dir
            .ancestors()
            .find(|a| a.metadata().is_ok_and(|m| !m.is_dir()));
        let err = anyhow::Error::new(e).context(format!("create {}", dir.display()));
        match blocker {
            Some(file) => err.context(format!(
                "{} is a file, not a directory; move it, or pick another \
                 directory with --workdir and --output-dir",
                file.display()
            )),
            None => err,
        }
    })
}

/// Accepts a source unchecked, saying so (`--allow-unverified`).
struct TlsOnly;

#[async_trait::async_trait]
impl nocloud_import::SourceCheck for TlsOnly {
    async fn check(&self, _file: &std::path::Path, _sha256_hex: &str) -> Result<()> {
        eprintln!("WARNING: --allow-unverified: no digest to check; trusting TLS alone");
        Ok(())
    }
}

async fn import_cmd(mut args: ImportArgs) -> Result<()> {
    use nocloud_import::host;

    let http = http_client()?;
    // Owned, since `args` is updated below when it names a UUID.
    let base = &args.index.trim_end_matches('/').to_string();
    let index = client::Client::new_with_client(base, http.clone());
    // A UUID from `avail` names a release's current image, default variant.
    if let Ok(uuid) = uuid::Uuid::parse_str(&args.distro) {
        anyhow::ensure!(
            args.release.is_none() && args.variant.is_none(),
            "an image UUID names the release and variant itself; give no release \
             or --variant with it"
        );
        let rows = available(&index, base, None).await?;
        let row = import::find_by_uuid(&rows, uuid).with_context(|| {
            format!(
                "no image in the index has UUID {uuid} (see `cloud-image avail`; \
                 an image whose vendor publishes no sha256 has no UUID until \
                 it is downloaded)"
            )
        })?;
        args.distro = row.distro.clone();
        args.release = Some(row.release.clone());
    }
    let distro = fetch_distro(&index, base, &args.distro).await?;
    let Some(release) = args.release.as_deref() else {
        anyhow::bail!(
            "name a release or alias of {} to import:\n{}",
            args.distro,
            import::release_choices(&distro).trim_end()
        );
    };
    let builds = match import::release_request(&distro, release)? {
        import::Requested::Alias("latest") => index.alias_latest(&args.distro).await,
        import::Requested::Alias("lts") => index.alias_lts(&args.distro).await,
        import::Requested::Alias("dev") => index.alias_dev(&args.distro).await,
        import::Requested::Alias(other) => {
            anyhow::bail!("alias {other:?} is not one this client knows how to fetch")
        }
        import::Requested::Release(release) => index.release_builds(&args.distro, release).await,
    }
    .map_err(|e| anyhow::anyhow!("{e}"))
    .with_context(|| format!("read {} {release} from the index at {base}", args.distro))?
    .into_inner();

    let (build, artifact) = import::choose_artifact(&builds, ARCH, args.variant.as_deref())?;
    let format = import::source_format(&artifact.format, &artifact.compression)?;
    let url = import::primary_url(artifact)?;
    let info = import::image_info(&distro, &builds.release, build, artifact);
    let digests = import::index_digests(artifact)?;

    println!(
        "Image:   {} {} build {}",
        info.vendor, info.series, info.version
    );
    println!("Source:  {url}");
    if import::needs_allow_unverified(&digests) && !args.allow_unverified {
        if digests.is_empty() {
            anyhow::bail!(
                "{} publishes no digest for this image; pass --allow-unverified to \
                 import it trusting TLS alone",
                info.vendor
            );
        }
        anyhow::bail!(
            "{} gives this image's digest only in a document with no generic \
             layout, so it cannot confirm the index's digest; pass \
             --allow-unverified to check the image against the index alone",
            info.vendor
        );
    }
    let (check, expected): (Box<dyn nocloud_import::SourceCheck>, _) = if digests.is_empty() {
        (Box::new(TlsOnly), Vec::new())
    } else {
        let (expected, notes) = import::confirm_with_vendor(&http, &digests).await?;
        for note in &notes {
            eprintln!("note: {note}");
        }
        let check = nocloud_import::DigestCheck(expected.clone());
        (Box::new(check), expected)
    };
    for digest in &expected {
        println!(
            "Expect:  {:?} {} ({})",
            digest.algorithm, digest.hex, digest.from
        );
    }
    match import::index_uuid(&digests) {
        Some(uuid) => println!("UUID:    {uuid}"),
        // The manifest UUID derives from the image's sha256.
        None => println!("UUID:    known after download (no sha256 is published)"),
    }

    if args.dry_run || !host::is_smartos()? {
        println!();
        println!("Dry run: nothing was downloaded, built or installed.");
        return Ok(());
    }

    let zone = host::current_zone()?;
    anyhow::ensure!(
        args.no_install || zone == "global",
        "imgadm install needs the global zone (zonename={zone}); pass --no-install \
         to build the files only"
    );
    let dataset = match args.dataset {
        Some(d) => d,
        None => host::default_dataset()?,
    };
    let stub = import::directory_name(&info);
    let workdir = args
        .workdir
        .unwrap_or_else(|| PathBuf::from(format!("/var/tmp/cloud-image/cache/{stub}")));
    let output_dir = args
        .output_dir
        .unwrap_or_else(|| PathBuf::from(format!("/var/tmp/cloud-image/image/{stub}")));
    create_dir(&workdir)?;
    // The pipeline creates it too, but would not say which path failed.
    create_dir(&output_dir)?;
    let _lock = host::acquire_workdir_lock(&workdir)?;

    let source = nocloud_import::Source { url, format };
    let outputs = nocloud_import::run(
        &source,
        &info,
        check.as_ref(),
        nocloud_import::PipelineOptions {
            workdir,
            output_dir,
            zfs_dataset: dataset,
            http: &http,
        },
    )
    .await?;
    println!();
    println!("Image:    {}", outputs.gz_path.display());
    println!("Manifest: {}", outputs.manifest_path.display());
    println!("UUID:     {}", outputs.manifest_uuid);
    if args.no_install {
        println!(
            "To install: imgadm install -m {} -f {}",
            outputs.manifest_path.display(),
            outputs.gz_path.display()
        );
    } else {
        host::install_via_imgadm(&outputs.gz_path, &outputs.manifest_path).await?;
        println!("Installed image {} with imgadm.", outputs.manifest_uuid);
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    match Args::parse().command {
        Command::Avail(args) => avail_cmd(args).await?,
        Command::Import(args) => import_cmd(args).await?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pipeline builds x86-64 bhyve images only, so there is no
    /// architecture to choose.
    #[test]
    fn import_takes_no_architecture() {
        assert!(
            Args::try_parse_from(["cloud-image", "import", "ubuntu", "--arch", "aarch64"]).is_err()
        );
        assert!(Args::try_parse_from(["cloud-image", "import", "ubuntu"]).is_ok());
    }

    /// A file where a directory should be (the binary, saved as
    /// /var/tmp/cloud-image) is named, rather than a bare "not a
    /// directory".
    #[test]
    fn a_file_in_the_way_of_a_directory_is_named() -> Result<()> {
        let base = std::env::temp_dir().join(format!("cloud-image-way-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base)?;
        let file = base.join("cloud-image");
        std::fs::write(&file, b"a binary")?;
        let err = create_dir(&file.join("cache").join("ubuntu"))
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(
            err.contains(&format!("{} is a file", file.display())),
            "{err}"
        );
        create_dir(&base.join("cache").join("ubuntu"))?;
        assert!(base.join("cache").join("ubuntu").is_dir());
        std::fs::remove_dir_all(&base)?;
        Ok(())
    }

    /// Without a release nothing is imported; the command fails so a
    /// script cannot mistake it for an import.
    #[test]
    fn the_release_is_optional_on_the_command_line() -> Result<()> {
        let args = Args::try_parse_from(["cloud-image", "import", "ubuntu"])?;
        let Command::Import(import) = args.command else {
            anyhow::bail!("not an import");
        };
        assert_eq!(import.release, None);
        Ok(())
    }

    /// imgadm's flags for scripts: -j for JSON, -H for no header.
    #[test]
    fn avail_takes_imgadms_flags() -> Result<()> {
        let args = Args::try_parse_from(["cloud-image", "avail", "ubuntu", "-j", "-H"])?;
        let Command::Avail(avail) = args.command else {
            anyhow::bail!("not avail");
        };
        assert_eq!(avail.distro.as_deref(), Some("ubuntu"));
        assert!(avail.json && avail.no_header);
        Ok(())
    }

    /// SmartOS has no system CA store that rustls can find, so the client
    /// trusts the bundled Mozilla roots, all of them.
    #[test]
    fn the_client_trusts_the_bundled_roots() -> Result<()> {
        let roots = bundled_roots()?;
        assert!(!roots.is_empty());
        assert_eq!(roots.len(), webpki_root_certs::TLS_SERVER_ROOT_CERTS.len());
        http_client()?;
        Ok(())
    }

    /// The UUIDs `avail` shows and `import` gives come from
    /// nocloud-import's derivation, which must stay tritonadm's, so both
    /// tools give one vendor image the same UUID. Expected value computed
    /// independently with Python's uuid module.
    #[test]
    fn manifest_uuid_matches_tritonadm() {
        assert_eq!(
            nocloud_import::stable_manifest_uuid(&"a".repeat(64)).to_string(),
            "15accdba-0a13-52fa-90d5-0cc27ecda43f"
        );
    }
}
