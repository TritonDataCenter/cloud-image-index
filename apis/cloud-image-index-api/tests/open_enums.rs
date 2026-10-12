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

/// The schema is a plain string that names the known values in its
/// description: no `enum`, which would reject later values, and no
/// `pattern` or `oneOf`, whose lookahead regex Go's RE2 cannot compile.
#[test]
fn schema_is_a_string_that_lists_the_known_values() {
    let schema = serde_json::to_value(schemars::schema_for!(Compression)).unwrap_or_default();
    assert_eq!(schema["type"], json!("string"), "{schema:#}");
    for absent in ["enum", "pattern", "oneOf", "anyOf"] {
        assert!(schema.get(absent).is_none(), "{absent} in {schema:#}");
    }
    let description = schema["description"].as_str().unwrap_or_default();
    for value in ["none", "gzip", "xz", "zstd", "bzip2"] {
        assert!(description.contains(&format!("`{value}`")), "{description}");
    }
    assert_eq!(Compression::schema_name(), "Compression");
}
