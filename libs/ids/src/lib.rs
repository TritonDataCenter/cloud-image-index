// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The rule for ids in the cloud-image-index. It lives in its own crate,
//! with no dependencies, so the format crate, the resolvers and the
//! client can share it without the client pulling in the format crate's
//! server dependencies.

/// The longest id the index publishes.
pub const MAX_ID_LEN: usize = 64;

/// Whether `id` is a valid distro, release, build or variant id: 1 to
/// [`MAX_ID_LEN`] characters from `[A-Za-z0-9._-]`, never containing
/// `..`. Clients name files and directories after these ids, and the
/// generator puts release tokens into vendor URLs, so none may carry a
/// path separator or climb out of a directory.
pub fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && !id.contains("..")
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_the_index_uses_are_valid() {
        for id in [
            "noble",
            "9",
            "1.14",
            "9.8-20260525.0",
            "v7.9_2026-08-31-22-45",
            "45_Beta",
        ] {
            assert!(is_valid_id(id), "{id:?}");
        }
        assert!(is_valid_id(&"a".repeat(MAX_ID_LEN)));
    }

    #[test]
    fn ids_that_could_name_another_path_are_not() {
        for id in ["", "..", "a..b", "../etc", "a/b", "a\\b", "a b", "\u{e9}"] {
            assert!(!is_valid_id(id), "{id:?}");
        }
        assert!(!is_valid_id(&"a".repeat(MAX_ID_LEN + 1)));
    }
}
