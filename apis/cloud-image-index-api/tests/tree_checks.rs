// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Each check in `tree::validate` must catch the breakage it exists
//! for. Every test copies the example tree to a scratch directory,
//! breaks one thing, and expects a specific problem.

use std::path::{Path, PathBuf};

use cloud_image_index_api::tree;

fn examples_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples")
}

fn copy_dir(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| format!("mkdir {to:?}: {e}"))?;
    for entry in std::fs::read_dir(from).map_err(|e| format!("read_dir {from:?}: {e}"))? {
        let path = entry.map_err(|e| format!("entry in {from:?}: {e}"))?.path();
        let dest = to.join(path.file_name().ok_or("entry without a name")?);
        if path.is_dir() {
            copy_dir(&path, &dest)?;
        } else {
            std::fs::copy(&path, &dest).map_err(|e| format!("copy {path:?}: {e}"))?;
        }
    }
    Ok(())
}

/// A copy of the example tree in a fresh scratch directory, removed on
/// drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Result<Self, String> {
        let dir = std::env::temp_dir().join(format!(
            "cloud-image-index-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        copy_dir(&examples_root(), &dir)?;
        Ok(Scratch(dir))
    }

    fn edit_json(&self, rel: &str, f: impl FnOnce(&mut serde_json::Value)) -> Result<(), String> {
        let path = self.0.join(rel);
        let text = std::fs::read_to_string(&path).map_err(|e| format!("read {path:?}: {e}"))?;
        let mut value: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("parse {path:?}: {e}"))?;
        f(&mut value);
        let text = serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?;
        std::fs::write(&path, text).map_err(|e| format!("write {path:?}: {e}"))
    }

    fn expect_problem(&self, needle: &str) -> Result<(), String> {
        match tree::validate(&self.0) {
            Ok(_) => Err(format!(
                "expected a problem containing {needle:?}, got none"
            )),
            Err(problems) if problems.iter().any(|p| p.contains(needle)) => Ok(()),
            Err(problems) => Err(format!(
                "expected a problem containing {needle:?}, got:\n{}",
                problems.join("\n")
            )),
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn unmodified_copy_is_valid() -> Result<(), String> {
    let s = Scratch::new("unmodified")?;
    tree::validate(&s.0).map_err(|p| p.join("\n"))?;
    Ok(())
}

#[test]
fn unknown_field_is_caught() -> Result<(), String> {
    let s = Scratch::new("unknown-field")?;
    s.edit_json("v1/distros/talos/index.json", |v| {
        v["bogus"] = serde_json::json!(1);
    })?;
    s.expect_problem("does not round-trip")
}

#[test]
fn alias_file_differing_from_release_is_caught() -> Result<(), String> {
    let s = Scratch::new("alias-differs")?;
    s.edit_json("v1/distros/ubuntu/aliases/lts.json", |v| {
        v["builds"][0]["build"] = serde_json::json!("x");
    })?;
    s.expect_problem("alias Lts file must equal resolute/index.json")
}

#[test]
fn stray_file_is_caught() -> Result<(), String> {
    let s = Scratch::new("stray-file")?;
    std::fs::write(s.0.join("v1/distros/talos/stray.json"), "{}").map_err(|e| e.to_string())?;
    s.expect_problem("is not a path the API defines")
}

#[test]
fn alias_without_file_is_caught() -> Result<(), String> {
    let s = Scratch::new("alias-no-file")?;
    std::fs::remove_file(s.0.join("v1/distros/ubuntu/aliases/lts.json"))
        .map_err(|e| e.to_string())?;
    s.expect_problem("alias Lts has no alias file")
}

#[test]
fn orphan_alias_file_is_caught() -> Result<(), String> {
    let s = Scratch::new("orphan-alias")?;
    std::fs::copy(
        s.0.join("v1/distros/talos/aliases/latest.json"),
        s.0.join("v1/distros/talos/aliases/dev.json"),
    )
    .map_err(|e| e.to_string())?;
    s.expect_problem("alias file Dev exists but no release holds that alias")
}

#[test]
fn archived_build_with_primary_location_is_caught() -> Result<(), String> {
    let s = Scratch::new("archive-primary")?;
    s.edit_json("v1/distros/rocky/releases/9/archive.json", |v| {
        v["builds"][0]["artifacts"][0]["locations"][0]["kind"] = serde_json::json!("primary");
    })?;
    s.expect_problem("archived build must not have a primary location")
}

#[test]
fn release_field_mismatch_is_caught() -> Result<(), String> {
    let s = Scratch::new("release-mismatch")?;
    s.edit_json("v1/distros/rocky/releases/8/index.json", |v| {
        v["release"] = serde_json::json!("9");
    })?;
    s.expect_problem("rocky/8: release field mismatch")
}

#[test]
fn unlisted_distro_is_caught() -> Result<(), String> {
    let s = Scratch::new("unlisted-distro")?;
    s.edit_json("v1/index.json", |v| {
        if let Some(list) = v["distros"].as_array_mut() {
            list.retain(|d| d["id"] != "talos");
        }
    })?;
    s.expect_problem("but distro directories are")
}

#[test]
fn unknown_enum_value_in_our_own_output_is_caught() -> Result<(), String> {
    // Clients must accept values they do not know, but the index itself
    // only publishes known ones, so a typo cannot slip out.
    let s = Scratch::new("unknown-enum")?;
    s.edit_json("v1/distros/rocky/releases/9/index.json", |v| {
        v["builds"][0]["artifacts"][0]["compression"] = serde_json::json!("gzipp");
    })?;
    s.expect_problem("unknown compression \"gzipp\"")
}

#[test]
fn missing_openapi_document_is_caught() -> Result<(), String> {
    let s = Scratch::new("openapi-missing")?;
    std::fs::remove_file(s.0.join("v1/openapi.json")).map_err(|e| e.to_string())?;
    s.expect_problem("missing v1/openapi.json")
}

#[test]
fn stale_openapi_document_is_caught() -> Result<(), String> {
    let s = Scratch::new("openapi-stale")?;
    s.edit_json("v1/openapi.json", |v| {
        v["info"]["title"] = "something else".into();
    })?;
    s.expect_problem("v1/openapi.json differs from the document this version generates")
}

#[test]
fn missing_landing_page_is_caught() -> Result<(), String> {
    let s = Scratch::new("index-html-missing")?;
    std::fs::remove_file(s.0.join("index.html")).map_err(|e| e.to_string())?;
    s.expect_problem("missing index.html")
}

#[test]
fn stale_landing_page_is_caught() -> Result<(), String> {
    let s = Scratch::new("index-html-stale")?;
    std::fs::write(s.0.join("index.html"), "<p>old</p>").map_err(|e| e.to_string())?;
    s.expect_problem("index.html differs from the page this version generates")
}

#[test]
fn lenient_load_reads_files_written_by_other_versions() -> Result<(), String> {
    // An older index lacks fields added since; a newer one has fields this
    // version does not know. Both must still be readable as the previous
    // tree, even though neither passes the strict checks for our output.
    let s = Scratch::new("lenient")?;
    s.edit_json("v1/distros/rocky/index.json", |v| {
        v.as_object_mut().map(|o| o.remove("homepage"));
    })?;
    s.edit_json("v1/distros/talos/index.json", |v| {
        v["added_later"] = serde_json::json!(1);
    })?;
    assert!(
        tree::validate(&s.0).is_err(),
        "strict validation must still object"
    );
    let loaded = tree::load_lenient(&s.0).map_err(|p| p.join("\n"))?;
    assert!(loaded.distros.contains_key("rocky"));
    assert!(loaded.distros.contains_key("talos"));
    Ok(())
}

#[test]
fn lenient_load_still_rejects_unreadable_files() -> Result<(), String> {
    let s = Scratch::new("lenient-corrupt")?;
    std::fs::write(s.0.join("v1/distros/rocky/index.json"), "not json")
        .map_err(|e| e.to_string())?;
    assert!(tree::load_lenient(&s.0).is_err());
    Ok(())
}
