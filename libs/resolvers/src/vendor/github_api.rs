// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Requests to the GitHub REST API (`api.github.com`), shared by the
//! vendors that discover releases there (Talos, OpenBSD).
//!
//! GitHub allows 60 unauthenticated requests an hour per IP address. CI
//! runners share addresses, so a job can find that budget already spent
//! and get `403 rate limit exceeded`. When `GITHUB_TOKEN` is set, its
//! value is sent as a bearer token, which raises the limit (to 1,000 an
//! hour per repository for a GitHub Actions job token). The requests
//! read public data only, so the token needs no permissions.
//!
//! The token goes only on requests built here, which are only for
//! `api.github.com` URLs. Asset and checksum downloads go elsewhere and
//! stay unauthenticated.

/// A GET of a GitHub API URL, with the API's media type, and with
/// `GITHUB_TOKEN` as a bearer token when it is set and not empty.
/// `url` must be an `https://api.github.com/` URL.
pub(crate) fn get(http: &reqwest::Client, url: &str) -> reqwest::RequestBuilder {
    get_with_token(http, url, std::env::var("GITHUB_TOKEN").ok().as_deref())
}

fn get_with_token(
    http: &reqwest::Client,
    url: &str,
    token: Option<&str>,
) -> reqwest::RequestBuilder {
    // GitHub requires a User-Agent on API requests; the caller's client
    // sets one (see the crate documentation).
    let req = http
        .get(url)
        .header("Accept", "application/vnd.github+json");
    match token {
        // bearer_auth marks the header sensitive.
        Some(token) if !token.is_empty() => req.bearer_auth(token),
        _ => req,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const URL: &str = "https://api.github.com/repos/siderolabs/talos/releases/latest";

    fn build(token: Option<&str>) -> reqwest::Request {
        let http = crate::test_client_builder().build().unwrap();
        get_with_token(&http, URL, token).build().unwrap()
    }

    #[test]
    fn sends_the_token_as_a_bearer_token() {
        let req = build(Some("abc123"));
        let auth = req.headers().get(reqwest::header::AUTHORIZATION).unwrap();
        assert_eq!(auth, "Bearer abc123");
        // Marked sensitive, so it is redacted in debug output.
        assert!(auth.is_sensitive());
    }

    #[test]
    fn no_token_sends_no_authorization() {
        assert!(
            build(None)
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .is_none()
        );
    }

    #[test]
    fn an_empty_token_sends_no_authorization() {
        // GitHub Actions sets an unavailable secret to the empty string.
        assert!(
            build(Some(""))
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .is_none()
        );
    }

    #[test]
    fn asks_for_the_api_media_type() {
        for token in [None, Some("abc123")] {
            let req = build(token);
            assert_eq!(req.method(), reqwest::Method::GET);
            assert_eq!(req.url().as_str(), URL);
            assert_eq!(
                req.headers().get(reqwest::header::ACCEPT).unwrap(),
                "application/vnd.github+json"
            );
        }
    }
}
