// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! `cloud-image self-update`: replace the running binary with one from a
//! GitHub release, as install.sh installs it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use url::Url;

/// The repository whose releases hold cloud-image.
pub const REPO: &str = "TritonDataCenter/cloud-image-index";

/// The binary's name among a release's files, as the illumos workflow
/// publishes it.
pub const ASSET: &str = "cloud-image-x86_64-unknown-illumos";

/// The version a release tag names: `v0.9.1` is 0.9.1.
pub fn tag_version(tag: &str) -> Result<semver::Version> {
    let version = tag
        .strip_prefix('v')
        .with_context(|| format!("release tag {tag:?} does not start with v"))?;
    semver::Version::parse(version)
        .with_context(|| format!("release tag {tag:?} is not v<version>"))
}

/// Whether to replace the running binary with a release's.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Update,
    UpToDate,
    /// This version is newer than the newest release, say a build from
    /// a branch; it is kept.
    Newer,
}

/// A release replaces this version when it is newer, or, when the user
/// named it, whenever it is a different version (to go back to it).
pub fn decide(current: &semver::Version, release: &semver::Version, named: bool) -> Decision {
    if release == current {
        Decision::UpToDate
    } else if !named && release < current {
        Decision::Newer
    } else {
        Decision::Update
    }
}

/// Where install.sh puts the man page for a binary at `exe`: beside the
/// binary's directory, `<prefix>/sbin/cloud-image` and
/// `<prefix>/man/man8/cloud-image.8.gz`.
pub fn installed_man_page(exe: &Path) -> Option<PathBuf> {
    let prefix = exe.parent()?.parent()?;
    Some(prefix.join("man").join("man8").join("cloud-image.8.gz"))
}

/// GitHub's API for the repository's releases.
pub fn releases_api() -> Result<Url> {
    Url::parse(&format!("https://api.github.com/repos/{REPO}/")).context("GitHub releases API URL")
}

/// The tag of the newest release, or of the release named `tag`, which
/// must exist; from the releases API at `api`.
pub async fn release_tag(http: &reqwest::Client, api: &Url, tag: Option<&str>) -> Result<String> {
    let url = match tag {
        None => api.join("releases/latest")?,
        Some(tag) => api.join(&format!("releases/tags/{tag}"))?,
    };
    let response = http
        .get(url.clone())
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if let Some(tag) = tag
        && response.status() == reqwest::StatusCode::NOT_FOUND
    {
        anyhow::bail!("there is no release {tag}");
    }
    let release: serde_json::Value = response
        .error_for_status()
        .with_context(|| format!("status from {url}"))?
        .json()
        .await
        .with_context(|| format!("parse {url}"))?;
    release
        .get("tag_name")
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .with_context(|| format!("{url} names no tag"))
}

/// Where a release's files are.
pub fn release_url(tag: &str) -> Result<Url> {
    Url::parse(&format!(
        "https://github.com/{REPO}/releases/download/{tag}/"
    ))
    .with_context(|| format!("release URL for {tag:?}"))
}

/// Download the release's binary (from `base`) next to `exe`, check it
/// against the release's SHA256SUMS and against its own `--version`,
/// then rename it over `exe`. Until the rename, the installed binary is
/// untouched; on any failure the download is removed.
pub async fn replace_binary(
    http: &reqwest::Client,
    base: &Url,
    exe: &Path,
    version: &semver::Version,
) -> Result<()> {
    let dir = exe
        .parent()
        .with_context(|| format!("{} has no directory", exe.display()))?;
    let new = dir.join(".cloud-image.new");
    let result = async {
        let url = base.join(ASSET)?;
        eprintln!("Downloading {url}");
        let bytes = http
            .get(url.clone())
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .with_context(|| format!("status from {url}"))?
            .bytes()
            .await
            .with_context(|| format!("read {url}"))?;
        tokio::fs::write(&new, &bytes)
            .await
            .with_context(|| format!("write {}", new.display()))?;

        let want = resolvers::verify::fetch_expected_hash(
            http,
            &base.join("SHA256SUMS")?,
            ASSET,
            resolvers::verify::SumsStyle::Gnu,
        )
        .await?;
        let got = resolvers::verify::sha256_file(&new).await?;
        anyhow::ensure!(
            got.eq_ignore_ascii_case(&want),
            "{ASSET} has sha256 {got}, but the release's SHA256SUMS says {want}"
        );

        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755))
            .await
            .with_context(|| format!("make {} executable", new.display()))?;
        let out = tokio::process::Command::new(&new)
            .arg("--version")
            .output()
            .await
            .with_context(|| format!("run {} --version", new.display()))?;
        let said = String::from_utf8_lossy(&out.stdout).trim().to_string();
        anyhow::ensure!(
            said == format!("cloud-image {version}"),
            "the downloaded binary says {said:?}, not \"cloud-image {version}\""
        );

        tokio::fs::rename(&new, exe)
            .await
            .with_context(|| format!("replace {}", exe.display()))
    }
    .await;
    if result.is_err() {
        // Failing to remove the download must not hide why the update
        // failed.
        let _ = tokio::fs::remove_file(&new).await;
    }
    result
}

/// Rewrite the man page install.sh put beside `exe`, with the binary now
/// at `exe`, so it matches that version. Returns the page, or `None` when
/// there is no installed page to refresh.
pub async fn refresh_man_page(exe: &Path) -> Result<Option<PathBuf>> {
    let Some(page) = installed_man_page(exe) else {
        return Ok(None);
    };
    if !tokio::fs::try_exists(&page).await.unwrap_or(false) {
        return Ok(None);
    }
    let dir = page
        .parent()
        .with_context(|| format!("{} has no directory", page.display()))?;
    let status = tokio::process::Command::new(exe)
        .arg("man")
        .arg("--out")
        .arg(dir)
        .status()
        .await
        .with_context(|| format!("run {} man", exe.display()))?;
    anyhow::ensure!(status.success(), "{} man exited {status}", exe.display());
    let status = tokio::process::Command::new("gzip")
        .arg("-f")
        .arg(dir.join("cloud-image.8"))
        .status()
        .await
        .context("run gzip")?;
    anyhow::ensure!(status.success(), "gzip exited {status}");
    Ok(Some(page))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_tag_names_a_version() -> anyhow::Result<()> {
        assert_eq!(tag_version("v0.9.1")?, semver::Version::new(0, 9, 1));
        assert!(tag_version("0.9.1").is_err(), "tags start with v");
        assert!(tag_version("vnext").is_err());
        Ok(())
    }

    /// Without a release named, only a newer one is installed; a named
    /// one is installed whatever its version, to go back to it.
    #[test]
    fn only_a_newer_release_replaces_this_one_unless_named() {
        let v = |s: &str| semver::Version::parse(s).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(decide(&v("0.9.0"), &v("0.9.1"), false), Decision::Update);
        assert_eq!(decide(&v("0.9.1"), &v("0.9.1"), false), Decision::UpToDate);
        assert_eq!(decide(&v("0.10.0"), &v("0.9.1"), false), Decision::Newer);
        assert_eq!(decide(&v("0.10.0"), &v("0.9.1"), true), Decision::Update);
        assert_eq!(decide(&v("0.9.1"), &v("0.9.1"), true), Decision::UpToDate);
    }

    /// A local stand-in for GitHub's releases API with one release, v0.9.1.
    async fn serve_api() -> anyhow::Result<url::Url> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        tokio::spawn(async move {
            while let Ok((mut conn, _)) = listener.accept().await {
                let mut request = [0u8; 2048];
                let n = conn.read(&mut request).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..n]).to_string();
                let found = request.starts_with("GET /releases/latest ")
                    || request.starts_with("GET /releases/tags/v0.9.1 ");
                let reply = if found {
                    let body = r#"{"tag_name": "v0.9.1"}"#;
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    )
                } else {
                    "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n".to_string()
                };
                let _ = conn.write_all(reply.as_bytes()).await;
            }
        });
        Ok(url::Url::parse(&format!("http://{addr}/"))?)
    }

    /// The newest release, or a named one only if it exists.
    #[tokio::test]
    async fn a_named_release_must_exist() -> anyhow::Result<()> {
        let api = serve_api().await?;
        let http = crate::http_client()?;
        assert_eq!(release_tag(&http, &api, None).await?, "v0.9.1");
        assert_eq!(release_tag(&http, &api, Some("v0.9.1")).await?, "v0.9.1");
        let err = release_tag(&http, &api, Some("v0.8.0")).await;
        assert!(
            format!("{:#}", err.err().unwrap_or_else(|| anyhow::anyhow!("none")))
                .contains("v0.8.0")
        );
        Ok(())
    }

    /// The man page sits where install.sh puts it, beside the binary's
    /// directory: <prefix>/sbin/cloud-image and <prefix>/man/man8.
    #[test]
    fn the_man_page_is_found_beside_the_binary() {
        assert_eq!(
            installed_man_page(std::path::Path::new("/opt/tools/sbin/cloud-image")),
            Some(std::path::PathBuf::from(
                "/opt/tools/man/man8/cloud-image.8.gz"
            ))
        );
        assert_eq!(
            installed_man_page(std::path::Path::new("cloud-image")),
            None
        );
    }

    /// A local "release": a stand-in binary that reports `version`, and
    /// its SHA256SUMS (or `sums`, when given).
    async fn serve_release(version: &str, sums: Option<&str>) -> anyhow::Result<url::Url> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let binary = format!("#!/bin/sh\necho \"cloud-image {version}\"\n");
        let hash = {
            use sha2::Digest;
            sha2::Sha256::digest(binary.as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        };
        let sums = sums
            .map(str::to_string)
            .unwrap_or_else(|| format!("{hash}  {ASSET}\n"));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        tokio::spawn(async move {
            while let Ok((mut conn, _)) = listener.accept().await {
                let mut request = [0u8; 2048];
                let n = conn.read(&mut request).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..n]).to_string();
                let body = if request.starts_with(&format!("GET /{ASSET} ")) {
                    binary.clone()
                } else if request.starts_with("GET /SHA256SUMS ") {
                    sums.clone()
                } else {
                    let _ = conn
                        .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n")
                        .await;
                    continue;
                };
                let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", body.len());
                let _ = conn.write_all(head.as_bytes()).await;
                let _ = conn.write_all(body.as_bytes()).await;
            }
        });
        Ok(url::Url::parse(&format!("http://{addr}/"))?)
    }

    fn scratch(name: &str) -> anyhow::Result<std::path::PathBuf> {
        let dir =
            std::env::temp_dir().join(format!("cloud-image-update-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sbin"))?;
        Ok(dir)
    }

    /// The release's binary replaces the installed one, after its digest
    /// and its own --version check out.
    #[tokio::test]
    async fn a_verified_release_replaces_the_binary() -> anyhow::Result<()> {
        let dir = scratch("ok")?;
        let exe = dir.join("sbin").join("cloud-image");
        std::fs::write(&exe, "old")?;
        let base = serve_release("9.9.9", None).await?;
        let http = crate::http_client()?;
        replace_binary(&http, &base, &exe, &semver::Version::new(9, 9, 9)).await?;
        let out = std::process::Command::new(&exe).output()?;
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "cloud-image 9.9.9"
        );
        assert!(!dir.join("sbin").join(".cloud-image.new").exists());
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    /// A binary that does not match SHA256SUMS, or reports another
    /// version, leaves the installed one alone.
    #[tokio::test]
    async fn a_bad_release_leaves_the_binary_alone() -> anyhow::Result<()> {
        let dir = scratch("bad")?;
        let exe = dir.join("sbin").join("cloud-image");
        let http = crate::http_client()?;

        std::fs::write(&exe, "old")?;
        let wrong_sums = format!("{}  {ASSET}\n", "0".repeat(64));
        let base = serve_release("9.9.9", Some(&wrong_sums)).await?;
        let err = replace_binary(&http, &base, &exe, &semver::Version::new(9, 9, 9)).await;
        assert!(
            format!("{:#}", err.err().unwrap_or_else(|| anyhow::anyhow!("none")))
                .contains("SHA256SUMS")
        );
        assert_eq!(std::fs::read_to_string(&exe)?, "old");

        let base = serve_release("1.2.3", None).await?;
        let err = replace_binary(&http, &base, &exe, &semver::Version::new(9, 9, 9)).await;
        assert!(
            format!("{:#}", err.err().unwrap_or_else(|| anyhow::anyhow!("none"))).contains("1.2.3")
        );
        assert_eq!(std::fs::read_to_string(&exe)?, "old");
        assert!(!dir.join("sbin").join(".cloud-image.new").exists());
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }
}
