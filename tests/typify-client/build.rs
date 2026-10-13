// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Generate the OpenAPI document from the API trait and run typify over
//! its schemas, as progenitor does when generating a client. Do the same
//! for the document 1.0 published (`fixtures/v1.0/openapi.json`).

use std::collections::BTreeMap;
use std::error::Error;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=../../apis/cloud-image-index-api/src");
    println!("cargo:rerun-if-changed=fixtures/v1.0/openapi.json");
    let out = PathBuf::from(std::env::var("OUT_DIR")?);
    typify(&api::openapi()?, &out.join("types.rs"))?;
    let v1_0: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string("fixtures/v1.0/openapi.json")?)?;
    typify(&v1_0, &out.join("types_v1_0.rs"))?;
    Ok(())
}

/// Write the types typify generates from `spec`'s schemas to `out`.
fn typify(spec: &serde_json::Value, out: &PathBuf) -> Result<(), Box<dyn Error>> {
    let schemas = spec
        .pointer("/components/schemas")
        .and_then(|s| s.as_object())
        .ok_or("OpenAPI document has no components/schemas")?;
    let mut defs = BTreeMap::new();
    for (name, schema) in schemas {
        let schema: schemars::schema::Schema = serde_json::from_value(schema.clone())?;
        defs.insert(name.clone(), schema);
    }
    let mut types = typify::TypeSpace::new(&typify::TypeSpaceSettings::default());
    types.add_ref_types(defs)?;
    std::fs::write(out, types.to_stream().to_string())?;
    Ok(())
}
