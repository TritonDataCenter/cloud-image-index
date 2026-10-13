// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! How a client generated from the published OpenAPI document copes
//! with the index changing under it, before the client is regenerated,
//! tried on the published image list, `v1/images.json`.
//!
//! - Extra fields are ignored.
//! - Optional fields may be missing.
//! - Enums whose value sets are expected to grow (image format,
//!   compression, checksum format, signature kind, alias) accept values
//!   the client does not know, and keep them.
//! - Enums whose value sets are closed (OS family, location kind, digest
//!   algorithm, firmware) reject unknown values.

use std::path::Path;

use cloud_image_index_typify_client::generated::ImageList;
use serde_json::{Value, json};

/// The example image list, with one change applied.
fn images_with(f: impl FnOnce(&mut Value)) -> Result<Value, String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/v1/images.json");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("read {path:?}: {e}"))?;
    let mut v: Value = serde_json::from_str(&text).map_err(|e| format!("parse {path:?}: {e}"))?;
    f(&mut v);
    Ok(v)
}

/// The entry for Rocky 9, which has variants and signatures.
fn rocky9(v: &mut Value) -> &mut Value {
    let images = v["images"].as_array_mut().map(|a| a.iter_mut());
    images
        .into_iter()
        .flatten()
        .find(|i| i["distro"]["id"] == "rocky" && i["release"]["id"] == "9")
        .unwrap_or_else(|| panic!("examples/v1/images.json has no rocky 9"))
}

fn artifact(v: &mut Value) -> &mut Value {
    &mut rocky9(v)["build"]["artifacts"][0]
}

fn parses(v: Value) -> Result<ImageList, String> {
    serde_json::from_value::<ImageList>(v).map_err(|e| e.to_string())
}

#[test]
fn the_example_parses_with_the_generated_types() -> Result<(), String> {
    let list = parses(images_with(|_| ())?)?;
    assert!(!list.images.is_empty());
    Ok(())
}

#[test]
fn extra_fields_are_ignored() -> Result<(), String> {
    let v = images_with(|v| {
        v["added_later"] = json!(true);
        rocky9(v)["added_later"] = json!(1);
        rocky9(v)["build"]["added_later"] = json!({"nested": 1});
        artifact(v)["added_later"] = json!([1, 2]);
        artifact(v)["integrity"]["added_later"] = json!("x");
    })?;
    parses(v)?;
    Ok(())
}

#[test]
fn optional_fields_may_be_missing() -> Result<(), String> {
    let v = images_with(|v| {
        for field in ["point_release", "published_at", "osinfo"] {
            rocky9(v)["build"].as_object_mut().map(|o| o.remove(field));
        }
        artifact(v).as_object_mut().map(|o| o.remove("size"));
        artifact(v)["integrity"]
            .as_object_mut()
            .map(|o| o.remove("http"));
        artifact(v)["integrity"]["digests"][0]
            .as_object_mut()
            .map(|o| o.remove("source"));
    })?;
    parses(v)?;
    Ok(())
}

/// Set a field to a value no client knows, parse, and check the value
/// survives a round trip through the generated type.
fn unknown_value_survives(
    set: impl FnOnce(&mut Value),
    get: fn(&mut Value) -> &mut Value,
) -> Result<(), String> {
    let mut v = images_with(set)?;
    let expected = get(&mut v).clone();
    let parsed = parses(v)?;
    let mut back = serde_json::to_value(&parsed).map_err(|e| e.to_string())?;
    let got = get(&mut back).clone();
    if got != expected {
        return Err(format!("value changed in round trip: {expected} -> {got}"));
    }
    Ok(())
}

#[test]
fn unknown_image_format_is_accepted_and_kept() -> Result<(), String> {
    unknown_value_survives(
        |v| artifact(v)["format"] = json!("vhd"),
        |v| &mut artifact(v)["format"],
    )
}

#[test]
fn unknown_compression_is_accepted_and_kept() -> Result<(), String> {
    unknown_value_survives(
        |v| artifact(v)["compression"] = json!("lz4"),
        |v| &mut artifact(v)["compression"],
    )
}

#[test]
fn unknown_checksum_format_is_accepted_and_kept() -> Result<(), String> {
    unknown_value_survives(
        |v| artifact(v)["integrity"]["digests"][0]["source"]["format"] = json!("sha3sums"),
        |v| &mut artifact(v)["integrity"]["digests"][0]["source"]["format"],
    )
}

#[test]
fn unknown_signature_kind_is_accepted_and_kept() -> Result<(), String> {
    unknown_value_survives(
        |v| artifact(v)["integrity"]["signatures"][0]["kind"] = json!("signify"),
        |v| &mut artifact(v)["integrity"]["signatures"][0]["kind"],
    )
}

#[test]
fn unknown_alias_is_accepted_and_kept() -> Result<(), String> {
    unknown_value_survives(
        |v| rocky9(v)["release"]["aliases"] = json!(["latest", "beta"]),
        |v| &mut rocky9(v)["release"]["aliases"],
    )
}

#[test]
fn unknown_digest_algorithm_is_accepted_and_kept() -> Result<(), String> {
    unknown_value_survives(
        |v| artifact(v)["integrity"]["digests"][0]["algorithm"] = json!("blake2b"),
        |v| &mut artifact(v)["integrity"]["digests"][0]["algorithm"],
    )
}

#[test]
fn unknown_location_kind_is_accepted_and_kept() -> Result<(), String> {
    unknown_value_survives(
        |v| artifact(v)["locations"][0]["kind"] = json!("cdn"),
        |v| &mut artifact(v)["locations"][0]["kind"],
    )
}

#[test]
fn unknown_firmware_is_accepted_and_kept() -> Result<(), String> {
    unknown_value_survives(
        |v| artifact(v)["firmware"] = json!(["openfirmware"]),
        |v| &mut artifact(v)["firmware"][0],
    )
}

#[test]
fn unknown_os_family_is_rejected() -> Result<(), String> {
    let v = images_with(|v| rocky9(v)["distro"]["os_family"] = json!("windows"))?;
    assert!(parses(v).is_err(), "os_family");
    Ok(())
}

/// Remove `fields` from an object, if present.
fn strip(v: &mut Value, fields: &[&str]) {
    if let Some(o) = v.as_object_mut() {
        for f in fields {
            o.remove(*f);
        }
    }
}

fn each(v: &mut Value, f: impl Fn(&mut Value)) {
    if let Some(items) = v.as_array_mut() {
        items.iter_mut().for_each(f);
    }
}

/// What a minimal, trusting client needs to find an image and ingest it
/// is required; everything else may be missing.
#[test]
fn documents_with_only_the_required_fields_parse() -> Result<(), String> {
    let v = images_with(|v| {
        each(&mut v["images"], |i| {
            strip(&mut i["distro"], &["name", "homepage"]);
            strip(
                &mut i["release"],
                &["version", "title", "aliases", "eol_date", "osinfo"],
            );
            strip(
                &mut i["build"],
                &["point_release", "published_at", "first_seen", "osinfo"],
            );
            each(&mut i["build"]["artifacts"], |a| {
                strip(
                    a,
                    &[
                        "variant",
                        "default_variant",
                        "firmware",
                        "datasources",
                        "ssh_key_injection",
                        "size",
                        "integrity",
                    ],
                );
                each(&mut a["locations"], |l| {
                    strip(l, &["kind", "redirects_off_host"])
                });
            });
        });
    })?;
    parses(v)?;
    Ok(())
}

/// Inside an optional structure, the fields that make it usable stay
/// required: a digest without a value is no digest.
#[test]
fn optional_structures_keep_their_load_bearing_fields() -> Result<(), String> {
    let v = images_with(|v| strip(&mut artifact(v)["integrity"]["digests"][0], &["value"]))?;
    assert!(parses(v).is_err(), "digest without value");
    let v = images_with(|v| strip(&mut artifact(v)["locations"][0], &["url"]))?;
    assert!(parses(v).is_err(), "location without url");
    let v = images_with(|v| strip(artifact(v), &["format"]))?;
    assert!(parses(v).is_err(), "artifact without format");
    Ok(())
}
