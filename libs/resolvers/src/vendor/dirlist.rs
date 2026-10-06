// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Shared "list a vendor's parent directory and pluck the numeric
//! subdirs" helper. Used by alma/rocky/centosstream release discovery,
//! which all enumerate available majors (or streams) by GETting a
//! parent index page and grepping for `href="<digits>/"`-shaped
//! anchors. The capture group in the regex picks the integer to
//! parse — callers supply both the URL and the regex so the shape
//! (`(\d+)/` vs `(\d+)-stream/`) stays explicit at each call site.
//!
//! Some hosts (`cloud.centos.org`, behind CloudFront) refuse requests
//! without a User-Agent; the caller's client must set one (see the crate
//! documentation).

use anyhow::{Context, Result};
use regex::Regex;

/// GET a parent index URL and parse out the sorted, deduped list of
/// numeric subdirectories matching `regex_pattern`. The regex must
/// capture the integer in group 1.
pub async fn fetch_numeric_subdirs(
    http: &reqwest::Client,
    base_url: &str,
    regex_pattern: &str,
) -> Result<Vec<u32>> {
    eprintln!("Fetching {base_url}");
    let body = http
        .get(base_url)
        .send()
        .await
        .with_context(|| format!("GET {base_url}"))?
        .error_for_status()
        .with_context(|| format!("status from {base_url}"))?
        .text()
        .await
        .with_context(|| format!("read body of {base_url}"))?;
    parse_numeric_subdirs(&body, regex_pattern)
}

/// Parse the integer captures of `regex_pattern` (group 1) out of an
/// HTML directory listing. Sorted and deduped. A regex compile failure
/// surfaces as an error so a typo in a const doesn't silently produce
/// "no majors found".
pub fn parse_numeric_subdirs(body: &str, regex_pattern: &str) -> Result<Vec<u32>> {
    let re = Regex::new(regex_pattern)
        .with_context(|| format!("invalid numeric-subdirs regex {regex_pattern:?}"))?;
    let mut nums: Vec<u32> = re
        .captures_iter(body)
        .filter_map(|c| c.get(1)?.as_str().parse().ok())
        .collect();
    nums.sort();
    nums.dedup();
    Ok(nums)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_alma_style_numeric_dirs() {
        let body = r#"
            <a href="8/">8/</a>
            <a href="9/">9/</a>
            <a href="10/">10/</a>
            <a href="vault/">vault/</a>
            <a href="testing/">testing/</a>
        "#;
        assert_eq!(
            parse_numeric_subdirs(body, r#"href="(\d+)/""#).unwrap(),
            vec![8, 9, 10]
        );
    }

    #[test]
    fn parses_centosstream_style_numeric_dirs() {
        let body = r#"
            <a href="8-stream/">8-stream/</a>
            <a href="9-stream/">9-stream/</a>
            <a href="10-stream/">10-stream/</a>
            <a href="archive/">archive/</a>
        "#;
        assert_eq!(
            parse_numeric_subdirs(body, r#"href="(\d+)-stream/""#).unwrap(),
            vec![8, 9, 10]
        );
    }

    #[test]
    fn dedupes_and_sorts() {
        let body = r#"href="9/" href="9/" href="8/""#;
        assert_eq!(
            parse_numeric_subdirs(body, r#"href="(\d+)/""#).unwrap(),
            vec![8, 9]
        );
    }

    #[test]
    fn invalid_regex_surfaces_error() {
        assert!(parse_numeric_subdirs("anything", "(unclosed").is_err());
    }
}
