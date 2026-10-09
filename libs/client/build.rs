// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Generate the client from the committed OpenAPI document,
//! `examples/v1/openapi.json`, which the tree checks keep identical to
//! the document the index publishes as `v1/openapi.json`.

use std::error::Error;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn Error>> {
    let spec_path = "../../examples/v1/openapi.json";
    println!("cargo:rerun-if-changed={spec_path}");
    let spec: openapiv3::OpenAPI = serde_json::from_str(&std::fs::read_to_string(spec_path)?)?;
    let mut settings = progenitor::GenerationSettings::default();
    settings.with_interface(progenitor::InterfaceStyle::Positional);
    let mut generator = progenitor::Generator::new(&settings);
    let tokens = generator.generate_tokens(&spec)?;
    let out = PathBuf::from(std::env::var("OUT_DIR")?).join("client.rs");
    std::fs::write(&out, tokens.to_string())?;
    Ok(())
}
