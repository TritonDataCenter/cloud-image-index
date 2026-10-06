// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Oracle Linux release discovery via the templates landing page at
//! `https://yum.oracle.com/oracle-linux-templates.html`.
//!
//! Oracle does not publish per-image checksum sidecars. Hashes live
//! embedded in the HTML, paired with image links per `<tr>` row:
//!
//! ```html
//! <a class="kvm-image" href=".../OL9U7_x86_64-kvm-b269.qcow2">…</a>
//! …
//! <tt class="kvm-sha256">88c75cf9…</tt>
//! ```
//!
//! For x86_64 the `kvm-b<build>.qcow2` template is the cloud-init
//! enabled one (Oracle's convention; aarch64 splits this into a
//! separate `kvm-cloud` variant, but x86_64 does not). We split the
//! HTML on `</tr>`, grab the first matching kvm-image link in each
//! row, and pair it with the row's `kvm-sha256` value.
//!
//! Trust roots in TLS to `yum.oracle.com`. Pinning the hex pulled
//! from the HTML at metadata time lets us emit a `Sha256Pinned`
//! verifier.

use anyhow::{Context, Result};
use regex::Regex;

pub const TEMPLATES_URL: &str = "https://yum.oracle.com/oracle-linux-templates.html";

#[derive(Debug, Clone)]
pub struct Resolved {
    pub major: String,
    pub update: String,
    pub build: String,
    pub url: String,
    pub sha256: String,
}

impl Resolved {
    /// Manifest version: `<major>.<update>-b<build>` (e.g. `9.7-b269`).
    pub fn version(&self) -> String {
        format!("{}.{}-b{}", self.major, self.update, self.build)
    }
}

pub async fn resolve(http: &reqwest::Client, release: &str) -> Result<Resolved> {
    let body = fetch_templates_html(http).await?;
    let rows = parse_rows(&body);
    if rows.is_empty() {
        anyhow::bail!("no Oracle Linux x86_64 kvm rows found at {TEMPLATES_URL}");
    }

    let token = release.trim();
    let target = if token.eq_ignore_ascii_case("latest") {
        rows.iter()
            .max_by_key(|r| r.major.parse::<u32>().unwrap_or(0))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no comparable Oracle majors at {TEMPLATES_URL}"))?
    } else {
        let major = parse_major(token)?;
        rows.iter()
            .find(|r| r.major == major)
            .cloned()
            .ok_or_else(|| {
                let available: Vec<&str> = rows.iter().map(|r| r.major.as_str()).collect();
                anyhow::anyhow!(
                    "oracle: major {major} not on the templates page (have: {})",
                    available.join(", ")
                )
            })?
    };
    Ok(target)
}

/// List every Oracle Linux x86_64 major on the templates page,
/// newest-first. Same page the resolver scrapes; one row per major.
pub async fn list(http: &reqwest::Client) -> Result<Vec<Resolved>> {
    let body = fetch_templates_html(http).await?;
    Ok(newest_first(parse_rows(&body)))
}

/// Rows ordered by major version, newest first.
fn newest_first(mut rows: Vec<Resolved>) -> Vec<Resolved> {
    rows.sort_by(|a, b| {
        b.major
            .parse::<u32>()
            .unwrap_or(0)
            .cmp(&a.major.parse::<u32>().unwrap_or(0))
    });
    rows
}

async fn fetch_templates_html(http: &reqwest::Client) -> Result<String> {
    eprintln!("Fetching Oracle Linux templates page ...");
    http.get(TEMPLATES_URL)
        .send()
        .await
        .with_context(|| format!("GET {TEMPLATES_URL}"))?
        .error_for_status()
        .with_context(|| format!("status from {TEMPLATES_URL}"))?
        .text()
        .await
        .with_context(|| format!("read body of {TEMPLATES_URL}"))
}

/// Split the HTML on `</tr>` and pull at most one row per major,
/// keeping the highest update/build seen for that major. The page
/// occasionally lists EOL releases (e.g. OL7) — they're returned
/// here too; the caller decides whether to surface them.
fn parse_rows(html: &str) -> Vec<Resolved> {
    let kvm_link = match Regex::new(
        r#"<a[^>]*class="kvm-image"[^>]*href="([^"]+/OL(\d+)/u(\d+)/x86_64/OL\d+U\d+_x86_64-kvm-b(\d+)\.qcow2)""#,
    ) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let kvm_hash = match Regex::new(r#"<tt[^>]*class="kvm-sha256"[^>]*>([0-9a-fA-F]{64})</tt>"#) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };

    let mut by_major: std::collections::BTreeMap<String, Resolved> =
        std::collections::BTreeMap::new();
    for chunk in html.split("</tr>") {
        let Some(link) = kvm_link.captures(chunk) else {
            continue;
        };
        let Some(hash) = kvm_hash.captures(chunk) else {
            continue;
        };
        let url = link
            .get(1)
            .map(|m| m.as_str())
            .unwrap_or_default()
            .to_string();
        let major = link
            .get(2)
            .map(|m| m.as_str())
            .unwrap_or_default()
            .to_string();
        let update = link
            .get(3)
            .map(|m| m.as_str())
            .unwrap_or_default()
            .to_string();
        let build = link
            .get(4)
            .map(|m| m.as_str())
            .unwrap_or_default()
            .to_string();
        let sha256 = hash
            .get(1)
            .map(|m| m.as_str().to_lowercase())
            .unwrap_or_default();
        if url.is_empty()
            || major.is_empty()
            || update.is_empty()
            || build.is_empty()
            || sha256.is_empty()
        {
            continue;
        }

        let entry = Resolved {
            major: major.clone(),
            update,
            build,
            url,
            sha256,
        };
        // Keep the highest update/build per major in case the page
        // ever lists multiple rows for the same major.
        match by_major.get(&major) {
            Some(existing) if compare_update_build(existing, &entry) >= 0 => {}
            _ => {
                by_major.insert(major, entry);
            }
        }
    }
    by_major.into_values().collect()
}

fn compare_update_build(a: &Resolved, b: &Resolved) -> i32 {
    let au: u32 = a.update.parse().unwrap_or(0);
    let bu: u32 = b.update.parse().unwrap_or(0);
    if au != bu {
        return if au > bu { 1 } else { -1 };
    }
    let ab: u32 = a.build.parse().unwrap_or(0);
    let bb: u32 = b.build.parse().unwrap_or(0);
    if ab != bb {
        if ab > bb { 1 } else { -1 }
    } else {
        0
    }
}

fn parse_major(input: &str) -> Result<String> {
    let s = input.trim().trim_start_matches(['o', 'O']);
    let s = s.trim_start_matches(['l', 'L']);
    if s.is_empty() || !s.chars().all(|c| c.is_ascii_digit()) {
        anyhow::bail!("oracle: expected a major version like '9' or 'latest', got {input:?}");
    }
    Ok(s.to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_rows_pairs_kvm_link_with_sha256() {
        let html = r#"
<tr id="ol9">
<td>9.7</td>
<td>
<ul>
<li><a class="kvm-image" href="https://yum.oracle.com/templates/OracleLinux/OL9/u7/x86_64/OL9U7_x86_64-kvm-b269.qcow2">…</a></li>
</ul>
</td>
<td>
<ul>
<li><small><tt class="olvm-sha256">a5cf3e2792a05138a8ea83c5739cca6dc38347a3f9523461644e2ae6ebb9d18e</tt></small></li>
<li><small><tt class="kvm-sha256">88c75cf913a66227e9ce74b0087ecac4cce1883f3e5649082e982d0d00310f1c</tt></small></li>
<li><small><tt class="vmdk-sha256">08950c9d63c698abdf8b53ab4788cd151647202e61e7d1fc0a70441938bda32f</tt></small></li>
</ul>
</td>
</tr>
<tr id="ol8">
<td>8.10</td>
<td>
<ul>
<li><a class="kvm-image" href="https://yum.oracle.com/templates/OracleLinux/OL8/u10/x86_64/OL8U10_x86_64-kvm-b271.qcow2">…</a></li>
</ul>
</td>
<td>
<ul>
<li><small><tt class="kvm-sha256">65077d1363f107cd750cdea26c73868c2128b5ed778ee93f0873aa2999228765</tt></small></li>
</ul>
</td>
</tr>
"#;
        let rows = parse_rows(html);
        assert_eq!(rows.len(), 2);

        let ol9 = rows.iter().find(|r| r.major == "9").unwrap();
        assert_eq!(ol9.update, "7");
        assert_eq!(ol9.build, "269");
        assert_eq!(
            ol9.sha256,
            "88c75cf913a66227e9ce74b0087ecac4cce1883f3e5649082e982d0d00310f1c"
        );
        assert_eq!(ol9.version(), "9.7-b269");

        let ol8 = rows.iter().find(|r| r.major == "8").unwrap();
        assert_eq!(ol8.update, "10");
        assert_eq!(ol8.build, "271");
    }

    #[test]
    fn parse_rows_skips_rows_missing_either_kvm_link_or_hash() {
        let html = r#"
<tr><td>OLVM only</td>
<td><a class="olvm-image" href="https://example/OL9U7_x86_64-olvm-b1.ova">…</a></td>
<td><tt class="olvm-sha256">aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa</tt></td>
</tr>
<tr><td>kvm without sha</td>
<td><a class="kvm-image" href="https://example/OL9/u7/x86_64/OL9U7_x86_64-kvm-b2.qcow2">…</a></td>
</tr>
"#;
        assert!(parse_rows(html).is_empty());
    }

    #[test]
    fn resolve_latest_picks_highest_major() {
        let html = r#"
<tr><a class="kvm-image" href="https://example/OL8/u10/x86_64/OL8U10_x86_64-kvm-b271.qcow2">…</a><tt class="kvm-sha256">65077d1363f107cd750cdea26c73868c2128b5ed778ee93f0873aa2999228765</tt></tr>
<tr><a class="kvm-image" href="https://example/OL9/u7/x86_64/OL9U7_x86_64-kvm-b269.qcow2">…</a><tt class="kvm-sha256">88c75cf913a66227e9ce74b0087ecac4cce1883f3e5649082e982d0d00310f1c</tt></tr>
<tr><a class="kvm-image" href="https://example/OL10/u1/x86_64/OL10U1_x86_64-kvm-b270.qcow2">…</a><tt class="kvm-sha256">23c72a22201b80c98195212e205c2ec0e2a641dfd5f37374dfe6e4f0639ef311</tt></tr>
"#;
        let rows = parse_rows(html);
        assert_eq!(rows.len(), 3);
        let target = rows
            .iter()
            .max_by_key(|r| r.major.parse::<u32>().unwrap_or(0))
            .unwrap();
        assert_eq!(target.major, "10");
    }

    #[test]
    fn parse_major_strips_optional_ol_prefix() {
        assert_eq!(parse_major("9").unwrap(), "9");
        assert_eq!(parse_major("OL9").unwrap(), "9");
        assert_eq!(parse_major("ol10").unwrap(), "10");
        assert!(parse_major("9.7").is_err());
        assert!(parse_major("nine").is_err());
        assert!(parse_major("").is_err());
    }

    #[test]
    fn newest_first_orders_majors_numerically() {
        let row = |major: &str| Resolved {
            major: major.to_string(),
            update: "1".to_string(),
            build: "1".to_string(),
            url: String::new(),
            sha256: String::new(),
        };
        let majors: Vec<String> = newest_first(vec![row("8"), row("10"), row("9")])
            .into_iter()
            .map(|r| r.major)
            .collect();
        assert_eq!(majors, ["10", "9", "8"]);
    }
}
