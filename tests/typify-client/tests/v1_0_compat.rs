// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The 1.0 promise: within `v1/`, the format changes only compatibly.
//! `fixtures/v1.0/` holds the published files as 1.0 shipped them, and
//! is never regenerated. A client generated from that OpenAPI document
//! must read what the index writes today, and a client generated from
//! today's document must read what 1.0 wrote.
//!
//! The documents read today are the examples, which the API crate's
//! tests keep equal in shape to what the generator writes.

use std::path::Path;

use cloud_image_index_typify_client::{generated, v1_0};
use serde::de::DeserializeOwned;

fn read<T: DeserializeOwned>(relative: &str) -> Result<T, String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    let text = std::fs::read_to_string(&path).map_err(|e| format!("read {path:?}: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("parse {path:?}: {e}"))
}

#[test]
fn a_1_0_client_reads_todays_image_list() -> Result<(), String> {
    let list: v1_0::ImageList = read("../../examples/v1/images.json")?;
    assert!(!list.images.is_empty());
    Ok(())
}

#[test]
fn a_1_0_client_reads_todays_distro_list() -> Result<(), String> {
    let list: v1_0::DistroList = read("../../examples/v1/index.json")?;
    assert!(!list.distros.is_empty());
    Ok(())
}

#[test]
fn todays_client_reads_the_1_0_image_list() -> Result<(), String> {
    let list: generated::ImageList = read("fixtures/v1.0/images.json")?;
    assert!(!list.images.is_empty());
    Ok(())
}

#[test]
fn todays_client_reads_the_1_0_distro_list() -> Result<(), String> {
    let list: generated::DistroList = read("fixtures/v1.0/index.json")?;
    assert!(!list.distros.is_empty());
    Ok(())
}
