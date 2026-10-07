// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The API's paths mirror a tree of static files. Dropshot does not
//! allow a literal path segment and a variable one at the same level,
//! so the tree keeps them apart (`distros/{distro}`, `aliases/...`,
//! `releases/{release}`). Check that Dropshot accepts the layout and
//! that every file in the tree is in the generated OpenAPI document.

use cloud_image_index_api::openapi;

#[test]
fn stub_api_description_builds_with_file_tree_paths() -> Result<(), String> {
    let spec = openapi()?;
    let paths = spec
        .get("paths")
        .and_then(|p| p.as_object())
        .ok_or("OpenAPI document has no paths")?;
    for expected in [
        "/v1/index.json",
        "/v1/openapi.json",
        "/v1/distros/{distro}/index.json",
        "/v1/distros/{distro}/aliases/latest.json",
        "/v1/distros/{distro}/aliases/lts.json",
        "/v1/distros/{distro}/aliases/dev.json",
        "/v1/distros/{distro}/releases/{release}/index.json",
        "/v1/distros/{distro}/releases/{release}/archive.json",
    ] {
        assert!(paths.contains_key(expected), "missing path {expected}");
    }
    assert_eq!(paths.len(), 8, "unexpected extra paths: {:?}", paths.keys());
    Ok(())
}

/// The meaning of each known open-enum value is part of the format, so
/// it must reach the published document, not only the Rust docs.
#[test]
fn open_enum_value_docs_are_published() -> Result<(), String> {
    let spec = openapi()?;
    let description = |name: &str| {
        spec.pointer(&format!("/components/schemas/{name}/description"))
            .and_then(|d| d.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let alias = description("Alias");
    assert!(
        alias.contains("`latest`: The newest generally-available release. Never a pre-release."),
        "{alias}"
    );
    let format = description("ChecksumFormat");
    assert!(format.contains("`gnu`: GNU coreutils style"), "{format}");
    // Values without docs of their own are still listed.
    let compression = description("Compression");
    assert!(compression.contains("`zstd`"), "{compression}");
    Ok(())
}

/// Dropshot takes an endpoint's first doc line as its summary, so each
/// endpoint's doc must open with a complete one-line sentence.
#[test]
fn endpoint_summaries_are_complete_sentences() -> Result<(), String> {
    let spec = openapi()?;
    let paths = spec
        .get("paths")
        .and_then(|p| p.as_object())
        .ok_or("OpenAPI document has no paths")?;
    for (path, item) in paths {
        let summary = item
            .pointer("/get/summary")
            .and_then(|s| s.as_str())
            .unwrap_or_default();
        assert!(summary.ends_with('.'), "{path}: summary {summary:?}");
    }
    Ok(())
}

/// The landing page is a file of the tree, so the trait defines it, but
/// it is for people, not part of the format: it stays out of the
/// published document.
#[test]
fn landing_page_is_not_in_the_published_document() -> Result<(), String> {
    let spec = openapi()?;
    assert!(spec.pointer("/paths/~1index.html").is_none());
    Ok(())
}

/// The landing page links to the published files by relative paths, so
/// it works at any base URL (GitHub Pages now, a custom domain later,
/// or a mirror).
#[test]
fn landing_page_links_are_relative() {
    let page = cloud_image_index_api::INDEX_HTML;
    for link in ["href=\"v1/index.json\"", "href=\"v1/openapi.json\""] {
        assert!(page.contains(link), "missing {link}");
    }
}

/// The API docs page is a file of the tree but, like the landing page,
/// not part of the format.
#[test]
fn docs_page_is_not_in_the_published_document() -> Result<(), String> {
    let spec = openapi()?;
    assert!(spec.pointer("/paths/~1docs~1index.html").is_none());
    Ok(())
}

/// The docs page renders the published spec, found relative to itself.
#[test]
fn docs_page_reads_the_published_spec() {
    assert!(cloud_image_index_api::DOCS_HTML.contains("\"../v1/openapi.json\""));
}

/// Every script and stylesheet the docs page loads from elsewhere is
/// pinned to an exact version and checked against a hash, so a changed
/// or compromised CDN file is refused rather than run on our site.
#[test]
fn docs_page_pins_and_hash_checks_everything_it_loads() {
    let page = cloud_image_index_api::DOCS_HTML;
    let mut external = 0;
    for tag in page
        .split('<')
        .filter(|t| t.starts_with("script") || t.starts_with("link"))
    {
        let tag = tag.split('>').next().unwrap_or_default();
        let Some(at) = tag.find("https://") else {
            continue;
        };
        external += 1;
        let url = tag[at..].split('"').next().unwrap_or_default();
        let version = url
            .split_once('@')
            .and_then(|(_, rest)| rest.split('/').next())
            .unwrap_or_default();
        let parts: Vec<&str> = version.split('.').collect();
        assert!(
            parts.len() == 3
                && parts
                    .iter()
                    .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())),
            "{url} is not pinned to an exact version"
        );
        assert!(
            tag.contains("integrity=\"sha384-"),
            "{url} has no integrity hash"
        );
        assert!(
            tag.contains("crossorigin=\"anonymous\""),
            "{url} lacks crossorigin"
        );
    }
    assert!(external > 0, "expected the page to load Swagger UI");
}

#[test]
fn landing_page_links_to_the_docs_page() {
    assert!(cloud_image_index_api::INDEX_HTML.contains("href=\"docs/\""));
}
