// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Generate the OpenAPI document from the API trait and run typify over
//! its schemas, as progenitor does when generating a client.

use std::collections::BTreeMap;
use std::error::Error;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=../../apis/cloud-image-index-api/src");
    let spec = api::openapi()?;
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
    let out = PathBuf::from(std::env::var("OUT_DIR")?).join("types.rs");
    std::fs::write(&out, types.to_stream().to_string())?;
    Ok(())
}
