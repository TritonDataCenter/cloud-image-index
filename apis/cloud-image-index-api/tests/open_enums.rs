// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Open enums: value sets expected to grow. Known values parse to their
//! variants, unknown values are kept in `Other`, and the schema tells
//! generated clients the same (`oneOf`: a known value, or any other
//! string).

use cloud_image_index_api::{Alias, ChecksumFormat, Compression, ImageFormat, SignatureKind};
use schemars::JsonSchema;
use serde_json::json;

#[test]
fn known_values_parse_to_their_variants() -> Result<(), serde_json::Error> {
    assert_eq!(
        serde_json::from_value::<Compression>(json!("zstd"))?,
        Compression::Zstd
    );
    assert_eq!(
        serde_json::from_value::<ImageFormat>(json!("qcow2"))?,
        ImageFormat::Qcow2
    );
    assert_eq!(
        serde_json::from_value::<ChecksumFormat>(json!("vendor_document"))?,
        ChecksumFormat::VendorDocument
    );
    assert_eq!(
        serde_json::from_value::<SignatureKind>(json!("pgp_detached"))?,
        SignatureKind::PgpDetached
    );
    assert_eq!(serde_json::from_value::<Alias>(json!("lts"))?, Alias::Lts);
    Ok(())
}

#[test]
fn unknown_values_are_kept() -> Result<(), serde_json::Error> {
    let c: Compression = serde_json::from_value(json!("lz4"))?;
    assert_eq!(c, Compression::Other("lz4".to_string()));
    assert_eq!(serde_json::to_value(&c)?, json!("lz4"));
    let a: Alias = serde_json::from_value(json!("beta"))?;
    assert_eq!(a, Alias::Other("beta".to_string()));
    Ok(())
}

#[test]
fn schema_is_a_known_value_or_any_other_string() {
    let schema = serde_json::to_value(schemars::schema_for!(Compression)).unwrap_or_default();
    let one_of = schema["oneOf"].as_array().cloned().unwrap_or_default();
    assert_eq!(one_of.len(), 2, "{schema:#}");
    assert_eq!(
        one_of[0]["enum"],
        json!(["none", "gzip", "xz", "zstd", "bzip2"])
    );
    // Any other string: a pattern that excludes the known values, so the
    // two branches are disjoint as `oneOf` requires. (typify turns a
    // `not: {enum}` branch into an empty enum, so `not` cannot be used.)
    assert_eq!(one_of[1]["type"], json!("string"));
    assert_eq!(
        one_of[1]["pattern"],
        json!("^(?!(?:none|gzip|xz|zstd|bzip2)$)")
    );
    assert_eq!(Compression::schema_name(), "Compression");
}
