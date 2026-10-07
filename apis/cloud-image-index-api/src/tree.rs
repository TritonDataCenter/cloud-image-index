// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Load an index file tree from disk and check it.
//!
//! Two ways to load:
//! - [`load`] / [`validate`]: strict, for what this version is about to
//!   publish. Every file must sit at a path the API defines and
//!   round-trip through its type without losing anything, and
//!   [`validate`] also checks the files agree with their paths and with
//!   the distro index. Used by the generator on its own output and by
//!   the tests of the hand-built examples.
//! - [`load_lenient`]: for a tree another version of the generator
//!   wrote (the previous output a run builds on). Files only need to
//!   parse and sit at an API path; missing optional fields and unknown
//!   fields are fine. No consistency checks: callers that rely on the
//!   tree being complete must check what they use.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::{
    Alias, BuildList, ChecksumFormat, Compression, DigestAlgorithm, Distro, DistroList,
    ImageFormat, LocationKind, SignatureKind,
};

/// An index tree as read from disk.
#[derive(Debug, Default)]
pub struct Tree {
    pub distro_list: Option<DistroList>,
    /// `v1/openapi.json`
    pub openapi: Option<serde_json::Value>,
    /// `index.html`
    pub index_html: Option<String>,
    /// `docs/index.html`
    pub docs_html: Option<String>,
    pub distros: BTreeMap<String, Distro>,
    /// (distro, release) -> current builds
    pub releases: BTreeMap<(String, String), BuildList>,
    /// (distro, release) -> archived builds
    pub archives: BTreeMap<(String, String), BuildList>,
    /// (distro, alias) -> alias file contents
    pub aliases: BTreeMap<(String, Alias), BuildList>,
}

/// Load the tree rooted at `root` (the directory containing `v1/`) and
/// check it. Returns every problem found, not just the first.
pub fn validate(root: &Path) -> Result<Tree, Vec<String>> {
    let tree = load(root)?;
    let problems = check(&tree);
    if problems.is_empty() {
        Ok(tree)
    } else {
        Err(problems)
    }
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("read_dir {dir:?}: {e}"))?;
    for entry in entries {
        let path = entry
            .map_err(|e| format!("dir entry in {dir:?}: {e}"))?
            .path();
        if path.is_dir() {
            collect_files(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

/// How strictly files are read.
#[derive(Clone, Copy)]
enum Strictness {
    /// Re-serializing must give back exactly the original JSON, so no
    /// field is silently ignored and none is missing. For checking what
    /// this version is about to publish.
    RoundTrip,
    /// Anything the types can parse: missing optional fields and unknown
    /// fields are fine. For reading a tree another version wrote.
    Parse,
}

/// Parse `path` as `T`, checking it as strictly as `strictness` says.
fn read_file<T: DeserializeOwned + Serialize>(
    path: &Path,
    strictness: Strictness,
) -> Result<T, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("read {path:?}: {e}"))?;
    let original: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("{path:?} is not JSON: {e}"))?;
    let parsed: T = serde_json::from_value(original.clone())
        .map_err(|e| format!("{path:?} does not match its type: {e}"))?;
    if let Strictness::Parse = strictness {
        return Ok(parsed);
    }
    let reserialized =
        serde_json::to_value(&parsed).map_err(|e| format!("re-serialize {path:?}: {e}"))?;
    if reserialized != original {
        return Err(format!(
            "{path:?} does not round-trip; fields were dropped or altered"
        ));
    }
    Ok(parsed)
}

fn alias_from_file(name: &str) -> Option<Alias> {
    match name {
        "latest.json" => Some(Alias::Latest),
        "lts.json" => Some(Alias::Lts),
        "dev.json" => Some(Alias::Dev),
        _ => None,
    }
}

/// Read every file under `root`, placing each by its path, with the
/// strict round-trip check used for this version's own output.
pub fn load(root: &Path) -> Result<Tree, Vec<String>> {
    load_with(root, Strictness::RoundTrip)
}

/// Read a tree that another version of the generator may have written:
/// files may lack fields added since, or carry fields added later. Every
/// file must still parse and sit at a path the API defines.
pub fn load_lenient(root: &Path) -> Result<Tree, Vec<String>> {
    load_with(root, Strictness::Parse)
}

fn load_with(root: &Path, strictness: Strictness) -> Result<Tree, Vec<String>> {
    let mut files = Vec::new();
    collect_files(root, &mut files).map_err(|e| vec![e])?;
    let mut tree = Tree::default();
    let mut problems = Vec::new();
    for path in files {
        let rel = match path.strip_prefix(root) {
            Ok(rel) => rel,
            Err(e) => {
                problems.push(format!("strip prefix {path:?}: {e}"));
                continue;
            }
        };
        let parts: Vec<&str> = rel.iter().filter_map(|s| s.to_str()).collect();
        let result = match parts.as_slice() {
            ["v1", "index.json"] => {
                read_file(&path, strictness).map(|v| tree.distro_list = Some(v))
            }
            ["index.html"] => std::fs::read_to_string(&path)
                .map(|v| tree.index_html = Some(v))
                .map_err(|e| format!("read {path:?}: {e}")),
            ["docs", "index.html"] => std::fs::read_to_string(&path)
                .map(|v| tree.docs_html = Some(v))
                .map_err(|e| format!("read {path:?}: {e}")),
            ["v1", "openapi.json"] => read_file(&path, strictness).map(|v| tree.openapi = Some(v)),
            ["v1", "distros", distro, "index.json"] => read_file(&path, strictness).map(|v| {
                tree.distros.insert((*distro).to_string(), v);
            }),
            ["v1", "distros", distro, "aliases", file] if alias_from_file(file).is_some() => {
                match alias_from_file(file) {
                    Some(alias) => read_file(&path, strictness).map(|v| {
                        tree.aliases.insert(((*distro).to_string(), alias), v);
                    }),
                    None => Ok(()),
                }
            }
            ["v1", "distros", distro, "releases", release, "index.json"] => {
                read_file(&path, strictness).map(|v| {
                    tree.releases
                        .insert(((*distro).to_string(), (*release).to_string()), v);
                })
            }
            ["v1", "distros", distro, "releases", release, "archive.json"] => {
                read_file(&path, strictness).map(|v| {
                    tree.archives
                        .insert(((*distro).to_string(), (*release).to_string()), v);
                })
            }
            _ => Err(format!("{rel:?} is not a path the API defines")),
        };
        if let Err(e) = result {
            problems.push(e);
        }
    }
    if problems.is_empty() {
        Ok(tree)
    } else {
        Err(problems)
    }
}

/// Check that the files of a loaded tree agree with each other.
pub fn check(tree: &Tree) -> Vec<String> {
    let mut problems = Vec::new();

    let Some(distro_list) = &tree.distro_list else {
        problems.push("missing v1/index.json".to_string());
        return problems;
    };
    match &tree.index_html {
        None => problems.push("missing index.html".to_string()),
        Some(page) if page != crate::INDEX_HTML => problems.push(
            "index.html differs from the page this version generates (after \
                 changing it, copy apis/cloud-image-index-api/src/index.html to \
                 examples/index.html)"
                .to_string(),
        ),
        Some(_) => {}
    }
    match &tree.docs_html {
        None => problems.push("missing docs/index.html".to_string()),
        Some(page) if page != crate::DOCS_HTML => problems.push(
            "docs/index.html differs from the page this version generates (after \
             changing it, copy apis/cloud-image-index-api/src/docs.html to \
             examples/docs/index.html)"
                .to_string(),
        ),
        Some(_) => {}
    }
    match (&tree.openapi, crate::openapi()) {
        (None, _) => problems.push("missing v1/openapi.json".to_string()),
        (Some(_), Err(e)) => problems.push(e),
        (Some(published), Ok(generated)) if *published != generated => problems.push(
            "v1/openapi.json differs from the document this version generates \
             (after changing the API, refresh examples/v1/openapi.json with \
             `cloud-image-index-generate openapi`)"
                .to_string(),
        ),
        (Some(_), Ok(_)) => {}
    }

    let listed: BTreeSet<&str> = distro_list.distros.iter().map(|d| d.id.as_str()).collect();
    let present: BTreeSet<&str> = tree.distros.keys().map(String::as_str).collect();
    if listed != present {
        problems.push(format!(
            "v1/index.json lists {listed:?} but distro directories are {present:?}"
        ));
    }

    for (id, distro) in &tree.distros {
        if &distro.id != id {
            problems.push(format!(
                "{id}: distro id {:?} differs from its directory",
                distro.id
            ));
        }
        if let Some(summary) = distro_list.distros.iter().find(|d| &d.id == id) {
            if summary.name != distro.name {
                problems.push(format!("{id}: name differs from v1/index.json"));
            }
            if summary.os_family != distro.os_family {
                problems.push(format!("{id}: os_family differs from v1/index.json"));
            }
            if summary.homepage != distro.homepage {
                problems.push(format!("{id}: homepage differs from v1/index.json"));
            }
        }

        let mut seen_releases = BTreeSet::new();
        for release in &distro.releases {
            if !seen_releases.insert(release.id.as_str()) {
                problems.push(format!(
                    "{id}: release {:?} listed more than once",
                    release.id
                ));
            }
        }
        let holds_dev = distro
            .releases
            .iter()
            .any(|r| r.aliases.contains(&Alias::Dev));
        match (holds_dev, &distro.dev_channel) {
            (true, None) => problems.push(format!(
                "{id}: a release holds the dev alias but dev_channel is null"
            )),
            (false, Some(_)) => problems.push(format!(
                "{id}: dev_channel is set but no release holds the dev alias"
            )),
            _ => {}
        }

        let mut seen_aliases = BTreeSet::new();
        for release in &distro.releases {
            let key = (id.clone(), release.id.clone());
            let target = tree.releases.get(&key);
            if target.is_none() {
                problems.push(format!(
                    "{id}/{}: listed release has no index.json",
                    release.id
                ));
            }
            for alias in &release.aliases {
                if !seen_aliases.insert(alias.clone()) {
                    problems.push(format!(
                        "{id}: alias {alias:?} held by more than one release"
                    ));
                }
                match tree.aliases.get(&(id.clone(), alias.clone())) {
                    None => problems.push(format!("{id}: alias {alias:?} has no alias file")),
                    Some(alias_file) if Some(alias_file) != target => {
                        problems.push(format!(
                            "{id}: alias {alias:?} file must equal {}/index.json",
                            release.id
                        ));
                    }
                    Some(_) => {}
                }
            }
        }
        for (alias_distro, alias) in tree.aliases.keys() {
            if alias_distro == id && !seen_aliases.contains(alias) {
                problems.push(format!(
                    "{id}: alias file {alias:?} exists but no release holds that alias"
                ));
            }
        }
    }

    for (alias_distro, alias) in tree.aliases.keys() {
        if !tree.distros.contains_key(alias_distro) {
            problems.push(format!(
                "{alias_distro}: alias file {alias:?} for a distro not in the index"
            ));
        }
    }

    for ((distro, release), list) in tree.releases.iter().chain(&tree.archives) {
        check_builds(&mut problems, distro, release, list);
    }

    for ((distro, release), list) in &tree.releases {
        check_build_list_names(&mut problems, distro, release, list);
        if !tree
            .distros
            .get(distro)
            .is_some_and(|d| d.releases.iter().any(|r| &r.id == release))
        {
            problems.push(format!(
                "{distro}/{release}: release directory not listed in distro index"
            ));
        }
        for build in &list.builds {
            for artifact in &build.artifacts {
                if !artifact
                    .locations
                    .iter()
                    .any(|l| l.kind == Some(LocationKind::Primary))
                {
                    problems.push(format!(
                        "{distro}/{release}/{}: current build needs a primary location",
                        build.build
                    ));
                }
            }
        }
    }

    for (id, distro) in &tree.distros {
        for release in &distro.releases {
            for alias in &release.aliases {
                if let Alias::Other(v) = alias {
                    problems.push(format!("{id}/{}: unknown alias {v:?}", release.id));
                }
            }
        }
    }
    for ((distro, release), list) in tree.releases.iter().chain(&tree.archives) {
        check_known_values(&mut problems, distro, release, list);
    }

    for ((distro, release), list) in &tree.archives {
        check_build_list_names(&mut problems, distro, release, list);
        if !tree
            .distros
            .get(distro)
            .is_some_and(|d| d.releases.iter().any(|r| &r.id == release))
        {
            problems.push(format!(
                "{distro}/{release}: archive for a release not listed in the distro index"
            ));
        }
        for build in &list.builds {
            for artifact in &build.artifacts {
                if artifact
                    .locations
                    .iter()
                    .any(|l| l.kind == Some(LocationKind::Primary))
                {
                    problems.push(format!(
                        "{distro}/{release}/{}: archived build must not have a primary location",
                        build.build
                    ));
                }
            }
        }
    }

    problems
}

/// Rules for one build list, current or archived: it has builds, each
/// build and each artifact within a build is listed once, every
/// artifact can be downloaded from somewhere, and digests are well
/// formed.
fn check_builds(problems: &mut Vec<String>, distro: &str, release: &str, list: &BuildList) {
    if list.builds.is_empty() {
        problems.push(format!("{distro}/{release}: no builds"));
    }
    let mut seen_builds = BTreeSet::new();
    for build in &list.builds {
        let at = format!("{distro}/{release}/{}", build.build);
        if !seen_builds.insert(build.build.as_str()) {
            problems.push(format!("{at}: build listed more than once"));
        }
        let mut seen_artifacts = BTreeSet::new();
        for artifact in &build.artifacts {
            let format = serde_json::to_value(&artifact.format)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default();
            let name = format!(
                "{}/{}/{format}",
                artifact.variant.as_deref().unwrap_or("-"),
                artifact.arch
            );
            if !seen_artifacts.insert(name.clone()) {
                problems.push(format!("{at}: artifact {name} listed more than once"));
            }
            if artifact.locations.is_empty() {
                problems.push(format!("{at}: artifact {name} has no locations"));
            }
            for digest in artifact.integrity.iter().flat_map(|i| &i.digests) {
                let len = match digest.algorithm {
                    DigestAlgorithm::Sha256 => 64,
                    DigestAlgorithm::Sha512 => 128,
                };
                let hex = digest
                    .value
                    .chars()
                    .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c));
                if !hex || digest.value.len() != len {
                    problems.push(format!(
                        "{at}: artifact {name} digest is not lowercase hex of the right length"
                    ));
                }
            }
        }
    }
}

/// Clients accept enum values they do not know, but the index itself
/// only publishes values this version defines, so a typo is caught here
/// rather than published.
fn check_known_values(problems: &mut Vec<String>, distro: &str, release: &str, list: &BuildList) {
    for build in &list.builds {
        let at = format!("{distro}/{release}/{}", build.build);
        for artifact in &build.artifacts {
            if let ImageFormat::Other(v) = &artifact.format {
                problems.push(format!("{at}: unknown format {v:?}"));
            }
            if let Compression::Other(v) = &artifact.compression {
                problems.push(format!("{at}: unknown compression {v:?}"));
            }
            let integrity = artifact.integrity.as_ref();
            for digest in integrity.iter().flat_map(|i| &i.digests) {
                if let Some(ChecksumFormat::Other(v)) = digest.source.as_ref().map(|s| &s.format) {
                    problems.push(format!("{at}: unknown checksum format {v:?}"));
                }
            }
            for signature in integrity.iter().flat_map(|i| &i.signatures) {
                if let SignatureKind::Other(v) = &signature.kind {
                    problems.push(format!("{at}: unknown signature kind {v:?}"));
                }
            }
        }
    }
}

fn check_build_list_names(
    problems: &mut Vec<String>,
    distro: &str,
    release: &str,
    list: &BuildList,
) {
    if list.distro != distro {
        problems.push(format!("{distro}/{release}: distro field mismatch"));
    }
    if list.release != release {
        problems.push(format!("{distro}/{release}: release field mismatch"));
    }
}
