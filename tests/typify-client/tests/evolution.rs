// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! How a client generated from the published OpenAPI document copes
//! with the index changing under it, before the client is regenerated.
//!
//! - Extra fields are ignored.
//! - Optional fields may be missing.
//! - Enums whose value sets are expected to grow (image format,
//!   compression, checksum format, signature kind, alias) accept values
//!   the client does not know, and keep them.
//! - Enums whose value sets are closed (OS family, location kind, digest
//!   algorithm, firmware) reject unknown values.

use std::path::{Path, PathBuf};

use cloud_image_index_typify_client::generated::{BuildList, Distro};
use serde_json::{Value, json};

fn examples() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/v1/distros")
}

fn read(rel: &str) -> Result<Value, String> {
    let path = examples().join(rel);
    let text = std::fs::read_to_string(&path).map_err(|e| format!("read {path:?}: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("parse {path:?}: {e}"))
}

/// A release's build list from the examples, with one change applied.
fn build_list_with(f: impl FnOnce(&mut Value)) -> Result<Value, String> {
    let mut v = read("rocky/releases/9/index.json")?;
    f(&mut v);
    Ok(v)
}

fn artifact(v: &mut Value) -> &mut Value {
    &mut v["builds"][0]["artifacts"][0]
}

fn parses_as_build_list(v: Value) -> Result<BuildList, String> {
    serde_json::from_value::<BuildList>(v).map_err(|e| e.to_string())
}

#[test]
fn every_example_parses_with_the_generated_types() -> Result<(), String> {
    for distro in ["rocky", "talos", "ubuntu"] {
        serde_json::from_value::<Distro>(read(&format!("{distro}/index.json"))?)
            .map_err(|e| format!("{distro}/index.json: {e}"))?;
    }
    for rel in [
        "rocky/releases/9/index.json",
        "rocky/releases/9/archive.json",
        "talos/releases/1.14/index.json",
        "ubuntu/releases/resolute/index.json",
    ] {
        parses_as_build_list(read(rel)?).map_err(|e| format!("{rel}: {e}"))?;
    }
    Ok(())
}

#[test]
fn extra_fields_are_ignored() -> Result<(), String> {
    let v = build_list_with(|v| {
        v["added_later"] = json!(true);
        v["builds"][0]["added_later"] = json!({"nested": 1});
        artifact(v)["added_later"] = json!([1, 2]);
        artifact(v)["integrity"]["added_later"] = json!("x");
    })?;
    parses_as_build_list(v)?;
    Ok(())
}

#[test]
fn optional_fields_may_be_missing() -> Result<(), String> {
    let v = build_list_with(|v| {
        for field in ["point_release", "published_at", "osinfo"] {
            v["builds"][0].as_object_mut().map(|o| o.remove(field));
        }
        artifact(v).as_object_mut().map(|o| o.remove("size"));
        artifact(v)["integrity"]
            .as_object_mut()
            .map(|o| o.remove("http"));
        artifact(v)["integrity"]["digests"][0]
            .as_object_mut()
            .map(|o| o.remove("source"));
    })?;
    parses_as_build_list(v)?;
    Ok(())
}

/// Set a field to a value no client knows, parse, and check the value
/// survives a round trip through the generated type.
fn unknown_value_survives(
    set: impl FnOnce(&mut Value),
    get: fn(&Value) -> &Value,
) -> Result<(), String> {
    let v = build_list_with(set)?;
    let expected = get(&v).clone();
    let parsed = parses_as_build_list(v)?;
    let back = serde_json::to_value(&parsed).map_err(|e| e.to_string())?;
    if get(&back) != &expected {
        return Err(format!(
            "value changed in round trip: {} -> {}",
            expected,
            get(&back)
        ));
    }
    Ok(())
}

#[test]
fn unknown_image_format_is_accepted_and_kept() -> Result<(), String> {
    unknown_value_survives(
        |v| artifact(v)["format"] = json!("vhd"),
        |v| &v["builds"][0]["artifacts"][0]["format"],
    )
}

#[test]
fn unknown_compression_is_accepted_and_kept() -> Result<(), String> {
    unknown_value_survives(
        |v| artifact(v)["compression"] = json!("lz4"),
        |v| &v["builds"][0]["artifacts"][0]["compression"],
    )
}

#[test]
fn unknown_checksum_format_is_accepted_and_kept() -> Result<(), String> {
    unknown_value_survives(
        |v| artifact(v)["integrity"]["digests"][0]["source"]["format"] = json!("sha3sums"),
        |v| &v["builds"][0]["artifacts"][0]["integrity"]["digests"][0]["source"]["format"],
    )
}

#[test]
fn unknown_signature_kind_is_accepted_and_kept() -> Result<(), String> {
    unknown_value_survives(
        |v| artifact(v)["integrity"]["signatures"][0]["kind"] = json!("signify"),
        |v| &v["builds"][0]["artifacts"][0]["integrity"]["signatures"][0]["kind"],
    )
}

#[test]
fn unknown_alias_is_accepted_and_kept() -> Result<(), String> {
    let mut v = read("ubuntu/index.json")?;
    v["releases"][0]["aliases"] = json!(["latest", "beta"]);
    let parsed = serde_json::from_value::<Distro>(v).map_err(|e| e.to_string())?;
    let back = serde_json::to_value(&parsed).map_err(|e| e.to_string())?;
    assert_eq!(back["releases"][0]["aliases"], json!(["latest", "beta"]));
    Ok(())
}

#[test]
fn unknown_values_in_closed_enums_are_rejected() -> Result<(), String> {
    let mut distro = read("rocky/index.json")?;
    distro["os_family"] = json!("windows");
    assert!(
        serde_json::from_value::<Distro>(distro).is_err(),
        "os_family"
    );

    let v = build_list_with(|v| artifact(v)["locations"][0]["kind"] = json!("cdn"))?;
    assert!(parses_as_build_list(v).is_err(), "location kind");

    let v =
        build_list_with(|v| artifact(v)["integrity"]["digests"][0]["algorithm"] = json!("md5"))?;
    assert!(parses_as_build_list(v).is_err(), "digest algorithm");

    let v = build_list_with(|v| artifact(v)["firmware"] = json!(["openfirmware"]))?;
    assert!(parses_as_build_list(v).is_err(), "firmware");
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
    let mut distro = read("rocky/index.json")?;
    strip(&mut distro, &["name", "homepage", "dev_channel"]);
    each(&mut distro["releases"], |r| {
        strip(r, &["version", "title", "aliases", "eol_date", "osinfo"])
    });
    let parsed =
        serde_json::from_value::<Distro>(distro.clone()).map_err(|e| format!("Distro: {e}"))?;
    let back = serde_json::to_value(&parsed).map_err(|e| e.to_string())?;
    assert_eq!(back["releases"][0]["id"], distro["releases"][0]["id"]);

    for rel in [
        "rocky/releases/9/index.json",
        "rocky/releases/9/archive.json",
    ] {
        let mut list = read(rel)?;
        each(&mut list["builds"], |b| {
            strip(
                b,
                &["point_release", "published_at", "first_seen", "osinfo"],
            );
            each(&mut b["artifacts"], |a| {
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
        parses_as_build_list(list).map_err(|e| format!("{rel}: {e}"))?;
    }
    Ok(())
}

/// Inside an optional structure, the fields that make it usable stay
/// required: a digest without a value is no digest.
#[test]
fn optional_structures_keep_their_load_bearing_fields() -> Result<(), String> {
    let v = build_list_with(|v| strip(&mut artifact(v)["integrity"]["digests"][0], &["value"]))?;
    assert!(parses_as_build_list(v).is_err(), "digest without value");
    let v = build_list_with(|v| strip(&mut artifact(v)["locations"][0], &["url"]))?;
    assert!(parses_as_build_list(v).is_err(), "location without url");
    let v = build_list_with(|v| strip(artifact(v), &["format"]))?;
    assert!(parses_as_build_list(v).is_err(), "artifact without format");
    Ok(())
}
