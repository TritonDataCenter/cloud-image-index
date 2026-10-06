// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Types typify generates from the index's OpenAPI document: what a
//! progenitor client built from the published spec would use. The tests
//! check how such a client copes when the index gains fields, drops
//! optional ones, or publishes enum values the client does not know.

// Generated code is not ours to lint; the workspace's unwrap/expect
// denials (restriction lints, outside clippy::all) are lifted here too.
#[allow(
    clippy::all,
    clippy::pedantic,
    clippy::unwrap_used,
    clippy::expect_used,
    unused,
    missing_docs
)]
pub mod generated {
    include!(concat!(env!("OUT_DIR"), "/types.rs"));
}
