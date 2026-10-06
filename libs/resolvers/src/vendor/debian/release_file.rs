// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Parse Debian's apt `Release` files. Each suite (`stable`,
//! `oldstable`, `trixie`, `bookworm`, ...) has one at
//! `https://deb.debian.org/debian/dists/<suite>/Release`. The header
//! is RFC822-style key/value pairs; the body following is package
//! lists and hashes that we don't care about. We only look at the
//! header fields and stop early.

use anyhow::{Context, Result};

const RELEASE_BASE_URL: &str = "https://deb.debian.org/debian/dists/";

pub struct ReleaseInfo {
    pub codename: String,
    pub version: String,
}

pub async fn fetch(http: &reqwest::Client, suite: &str) -> Result<ReleaseInfo> {
    let url = format!("{RELEASE_BASE_URL}{suite}/Release");
    eprintln!("Fetching Debian Release file ({suite}) ...");
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
    parse(&body).with_context(|| format!("parse {url}"))
}

/// Parse the RFC822-style header. Stops at the first line that
/// starts with whitespace (the start of a multi-line file-list field
/// like `MD5Sum:`), since everything we want is in the header.
pub fn parse(body: &str) -> Result<ReleaseInfo> {
    let mut codename: Option<String> = None;
    let mut version: Option<String> = None;
    for line in body.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            // Continuation / file-list block; we're past the header
            // fields we care about.
            break;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim() {
            "Codename" => codename = Some(value.trim().to_string()),
            "Version" => version = Some(value.trim().to_string()),
            _ => {}
        }
        if codename.is_some() && version.is_some() {
            break;
        }
    }
    let codename = codename.ok_or_else(|| anyhow::anyhow!("no Codename field in Release file"))?;
    let version = version.ok_or_else(|| anyhow::anyhow!("no Version field in Release file"))?;
    Ok(ReleaseInfo { codename, version })
}

/// Best-effort extraction of the major version integer from a
/// version string like `"13.4"` → `13`. Returns `None` if the string
/// doesn't start with digits.
pub fn major_of(version: &str) -> Option<u32> {
    let digits: String = version.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_typical_stable() {
        let body = "\
Origin: Debian
Label: Debian
Suite: stable
Version: 13.4
Codename: trixie
Date: Sat, 14 Mar 2026 11:25:23 UTC
Description: Debian 13.4 Released 14 March 2026
Architectures: all amd64 arm64 armel armhf i386 ppc64el riscv64 s390x
Components: main contrib non-free-firmware non-free
MD5Sum:
 abcd1234abcd1234abcd1234abcd1234            12345 main/source/Sources
 ...
";
        let info = parse(body).unwrap();
        assert_eq!(info.codename, "trixie");
        assert_eq!(info.version, "13.4");
    }

    #[test]
    fn parse_bookworm_oldstable() {
        let body = "\
Suite: oldstable
Codename: bookworm
Version: 12.7
Date: ...
";
        let info = parse(body).unwrap();
        assert_eq!(info.codename, "bookworm");
        assert_eq!(info.version, "12.7");
    }

    #[test]
    fn parse_missing_field_errors() {
        let body = "Origin: Debian\nSuite: stable\n";
        assert!(parse(body).is_err());
    }

    #[test]
    fn major_of_extracts_leading_int() {
        assert_eq!(major_of("13.4"), Some(13));
        assert_eq!(major_of("12"), Some(12));
        assert_eq!(major_of("11.7.0"), Some(11));
        assert_eq!(major_of(""), None);
        assert_eq!(major_of("abc"), None);
    }
}
