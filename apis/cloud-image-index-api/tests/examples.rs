// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Validate the hand-written example tree under `examples/` with the
//! same checks the generator applies to its output.

use std::path::{Path, PathBuf};

use cloud_image_index_api::tree;

fn examples_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples")
}

#[test]
fn examples_match_types_and_paths() -> Result<(), String> {
    tree::validate(&examples_root()).map_err(|problems| problems.join("\n"))?;
    Ok(())
}

#[test]
fn examples_cover_every_file_kind() -> Result<(), String> {
    let tree = tree::validate(&examples_root()).map_err(|problems| problems.join("\n"))?;
    assert!(tree.distro_list.is_some());
    assert!(tree.openapi.is_some());
    assert!(tree.images.is_some());
    assert!(tree.index_html.is_some());
    assert!(tree.docs_html.is_some());
    assert!(!tree.distros.is_empty());
    assert!(!tree.releases.is_empty());
    assert!(
        !tree.archives.is_empty(),
        "need at least one archive.json example"
    );
    assert!(!tree.aliases.is_empty());
    Ok(())
}
