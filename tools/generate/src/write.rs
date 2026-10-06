// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Write a generated index to disk.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result};
use api::tree::Tree;
use api::{Alias, Artifact, BuildList, Distro, DistroList, DistroSummary};
use serde::Serialize;

/// Keep `first_seen` stable: a build already present in the previous
/// output keeps the time it was first recorded, so unchanged builds do
/// not produce changed files.
pub fn preserve_first_seen(previous: Option<&BuildList>, list: &mut BuildList) {
    let Some(previous) = previous else {
        return;
    };
    for build in &mut list.builds {
        if let Some(old) = previous.builds.iter().find(|b| b.build == build.build) {
            build.first_seen = old.first_seen.or(build.first_seen);
        }
    }
}

/// Look up the previous output's build list for a release, if any.
pub fn previous_builds<'a>(
    previous: &'a Tree,
    distro: &str,
    release: &str,
) -> Option<&'a BuildList> {
    previous
        .releases
        .get(&(distro.to_string(), release.to_string()))
}

/// The previous output's artifact for `url` in a distro, from any
/// release's current or archived builds.
pub fn previous_artifact<'a>(previous: &'a Tree, distro: &str, url: &str) -> Option<&'a Artifact> {
    previous
        .releases
        .iter()
        .chain(previous.archives.iter())
        .filter(|((d, _), _)| d == distro)
        .flat_map(|(_, list)| &list.builds)
        .flat_map(|b| &b.artifacts)
        .find(|a| a.locations.iter().any(|l| l.url == url))
}

/// A distro exactly as the previous output had it, with its releases'
/// build lists in release order. Used for vendors that were not
/// regenerated this run (not selected, or failed), so a failure never
/// removes anything from the index. `Ok(None)` when the previous output
/// has no such distro; an error when it has the distro but not all of
/// its releases' files, rather than silently dropping the distro.
pub fn carry_over(previous: &Tree, distro: &str) -> Result<Option<(Distro, Vec<BuildList>)>> {
    let Some(d) = previous.distros.get(distro) else {
        return Ok(None);
    };
    let lists = d
        .releases
        .iter()
        .map(|r| {
            previous_builds(previous, distro, &r.id)
                .cloned()
                .with_context(|| {
                    format!(
                        "{distro}/{}: release listed but its index.json is missing",
                        r.id
                    )
                })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Some((d.clone(), lists)))
}

/// The published tree at `root`, as the previous output this run builds
/// on: for keeping known builds' `first_seen`, reusing metadata a request
/// could not fetch this time, and carrying over vendors without a fresh
/// fragment. `Ok(None)` when there is no tree yet. A tree that exists but
/// cannot be read is an error, never "no tree": treating it as absent
/// would drop every carried-over vendor and reset every `first_seen`.
/// It is read leniently, since an earlier version of the generator may
/// have written it.
pub fn load_previous(root: &Path) -> Result<Option<Tree>> {
    if !root.join("v1").exists() {
        return Ok(None);
    }
    let tree = api::tree::load_lenient(root).map_err(|problems| {
        anyhow::anyhow!(
            "previous tree {} cannot be read; refusing to build on it:\n  {}",
            root.display(),
            problems.join("\n  ")
        )
    })?;
    // Lenient loading checks no consistency, and carrying over walks
    // the distro directories, so a distro listed in v1/index.json
    // without its index.json would silently drop out.
    let listed: BTreeSet<&str> = tree
        .distro_list
        .as_ref()
        .with_context(|| {
            format!(
                "previous tree {} has no v1/index.json; refusing to build on it",
                root.display()
            )
        })?
        .distros
        .iter()
        .map(|d| d.id.as_str())
        .collect();
    let present: BTreeSet<&str> = tree.distros.keys().map(String::as_str).collect();
    if listed != present {
        anyhow::bail!(
            "previous tree {} lists distros {listed:?} but has distro indexes for \
             {present:?}; refusing to build on it",
            root.display()
        );
    }
    Ok(Some(tree))
}

/// Combine this run's freshly generated distros with the previous
/// output: each id in `order` takes its fresh entry if there is one,
/// otherwise the previous output's (see [`carry_over`]). Ids in neither
/// are left out, as are previous distros not in `order`. A previous
/// distro that cannot be carried over whole is an error.
pub fn merge(
    order: &[String],
    previous: Option<&Tree>,
    mut fresh: Vec<(Distro, Vec<BuildList>)>,
) -> Result<Vec<(Distro, Vec<BuildList>)>> {
    let mut merged = Vec::new();
    for id in order {
        match fresh.iter().position(|(d, _)| &d.id == id) {
            Some(i) => merged.push(fresh.swap_remove(i)),
            None => {
                if let Some(p) = previous
                    && let Some(d) = carry_over(p, id)
                        .with_context(|| format!("carrying over {id} from the previous tree"))?
                {
                    merged.push(d);
                }
            }
        }
    }
    Ok(merged)
}

/// Read a tree written by [`write_tree`] (a whole index, or one
/// vendor's fragment) back into distros with their build lists. The
/// tree must validate.
pub fn read_fragment(root: &Path) -> Result<Vec<(Distro, Vec<BuildList>)>> {
    let tree = api::tree::validate(root).map_err(|problems| {
        anyhow::anyhow!(
            "{} does not validate:\n  {}",
            root.display(),
            problems.join("\n  ")
        )
    })?;
    tree.distros
        .keys()
        .map(|id| {
            carry_over(&tree, id)
                .and_then(|d| d.context("distro vanished while reading"))
                .with_context(|| format!("{}: distro {id} is incomplete", root.display()))
        })
        .collect()
}

/// `v1/index.json` for a set of distros.
pub fn distro_list(distros: &[(Distro, Vec<BuildList>)]) -> DistroList {
    DistroList {
        distros: distros
            .iter()
            .map(|(d, _)| DistroSummary {
                id: d.id.clone(),
                name: d.name.clone(),
                os_family: d.os_family,
                homepage: d.homepage.clone(),
            })
            .collect(),
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
    }
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    std::fs::write(path, text).with_context(|| format!("write {}", path.display()))
}

/// The file an alias is published as. The index only publishes aliases
/// it defines.
fn alias_file(alias: &Alias) -> Result<&'static str> {
    match alias {
        Alias::Latest => Ok("latest.json"),
        Alias::Lts => Ok("lts.json"),
        Alias::Dev => Ok("dev.json"),
        Alias::Other(other) => anyhow::bail!("refusing to publish unknown alias {other:?}"),
    }
}

/// Replace `root/v1` with the given index. Each distro comes with the
/// build lists of its releases, in the same order as `distro.releases`.
pub fn write_tree(
    root: &Path,
    list: &DistroList,
    distros: &[(Distro, Vec<BuildList>)],
) -> Result<()> {
    let v1 = root.join("v1");
    if v1.exists() {
        std::fs::remove_dir_all(&v1).with_context(|| format!("remove {}", v1.display()))?;
    }
    std::fs::create_dir_all(root).with_context(|| format!("mkdir {}", root.display()))?;
    let page = root.join("index.html");
    std::fs::write(&page, api::INDEX_HTML).with_context(|| format!("write {}", page.display()))?;
    write_json(&v1.join("index.json"), list)?;
    write_json(
        &v1.join("openapi.json"),
        &api::openapi().map_err(anyhow::Error::msg)?,
    )?;
    for (distro, builds) in distros {
        if distro.releases.len() != builds.len() {
            anyhow::bail!(
                "{}: {} releases but {} build lists",
                distro.id,
                distro.releases.len(),
                builds.len()
            );
        }
        let dir = v1.join("distros").join(&distro.id);
        write_json(&dir.join("index.json"), distro)?;
        for (release, build_list) in distro.releases.iter().zip(builds) {
            write_json(
                &dir.join("releases").join(&release.id).join("index.json"),
                build_list,
            )?;
            for alias in &release.aliases {
                write_json(&dir.join("aliases").join(alias_file(alias)?), build_list)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use api::{Build, OsFamily, Release};
    use chrono::{DateTime, TimeZone, Utc};

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0)
            .single()
            .unwrap_or_else(|| panic!("bad timestamp"))
    }

    fn build(id: &str, first_seen: DateTime<Utc>) -> Build {
        Build {
            build: id.to_string(),
            point_release: None,
            published_at: None,
            first_seen: Some(first_seen),
            osinfo: None,
            artifacts: Vec::new(),
        }
    }

    fn list(builds: Vec<Build>) -> BuildList {
        BuildList {
            distro: "d".to_string(),
            release: "r".to_string(),
            builds,
        }
    }

    #[test]
    fn first_seen_is_kept_for_known_builds_only() {
        let previous = list(vec![build("old", at(100))]);
        let mut current = list(vec![build("new", at(200)), build("old", at(200))]);
        preserve_first_seen(Some(&previous), &mut current);
        assert_eq!(current.builds[0].first_seen, Some(at(200)));
        assert_eq!(current.builds[1].first_seen, Some(at(100)));
    }

    #[test]
    fn a_previous_build_without_first_seen_keeps_this_runs() {
        // Older trees, read leniently, may lack the field.
        let mut old = build("old", at(100));
        old.first_seen = None;
        let previous = list(vec![old]);
        let mut current = list(vec![build("old", at(200))]);
        preserve_first_seen(Some(&previous), &mut current);
        assert_eq!(current.builds[0].first_seen, Some(at(200)));
    }

    fn release(id: &str) -> Release {
        Release {
            id: id.to_string(),
            version: Some(id.to_string()),
            title: Some(id.to_string()),
            aliases: Vec::new(),
            eol_date: None,
            osinfo: None,
        }
    }

    #[test]
    fn carry_over_returns_previous_distro_with_build_lists_in_release_order() {
        let mut previous = Tree::default();
        previous.distros.insert(
            "d".to_string(),
            Distro {
                id: "d".to_string(),
                name: Some("D".to_string()),
                os_family: OsFamily::Linux,
                homepage: Some("https://d.example/".to_string()),
                dev_channel: None,
                releases: vec![release("2"), release("1")],
            },
        );
        for r in ["1", "2"] {
            let mut l = list(vec![build(r, at(1))]);
            l.release = r.to_string();
            previous
                .releases
                .insert(("d".to_string(), r.to_string()), l);
        }
        let (distro, lists) = carry_over(&previous, "d")
            .ok()
            .flatten()
            .unwrap_or_else(|| panic!("missing"));
        assert_eq!(distro.id, "d");
        assert_eq!(
            lists.iter().map(|l| l.release.as_str()).collect::<Vec<_>>(),
            vec!["2", "1"]
        );
        assert!(matches!(carry_over(&previous, "absent"), Ok(None)));
    }

    fn distro(id: &str, releases: &[&str]) -> (Distro, Vec<BuildList>) {
        let d = Distro {
            id: id.to_string(),
            name: Some(id.to_uppercase()),
            os_family: OsFamily::Linux,
            homepage: Some(format!("https://{id}.example/")),
            dev_channel: None,
            releases: releases
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    let mut rel = release(r);
                    if i == 0 {
                        rel.aliases = vec![Alias::Latest];
                    }
                    rel
                })
                .collect(),
        };
        let lists = releases
            .iter()
            .map(|r| BuildList {
                distro: id.to_string(),
                release: r.to_string(),
                builds: vec![build(&format!("{id}-{r}"), at(1))],
            })
            .collect();
        (d, lists)
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cloud-image-index-generate-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn merge_prefers_fresh_fragments_and_carries_over_the_rest_in_order() -> Result<()> {
        let prev_root = scratch("merge-prev");
        let previous = vec![distro("a", &["1"]), distro("b", &["1"])];
        write_tree(&prev_root, &distro_list(&previous), &previous)?;
        let prev_tree =
            api::tree::validate(&prev_root).map_err(|p| anyhow::anyhow!(p.join("\n")))?;
        let _ = std::fs::remove_dir_all(&prev_root);

        let order = ["c", "b", "a"].map(String::from);
        let merged = merge(&order, Some(&prev_tree), vec![distro("b", &["2", "1"])])?;
        let summary: Vec<(String, Vec<String>)> = merged
            .iter()
            .map(|(d, _)| {
                (
                    d.id.clone(),
                    d.releases.iter().map(|r| r.id.clone()).collect(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                ("b".to_string(), vec!["2".to_string(), "1".to_string()]),
                ("a".to_string(), vec!["1".to_string()]),
            ]
        );
        Ok(())
    }

    #[test]
    fn fragment_round_trips_through_disk() -> Result<()> {
        let root = scratch("fragment");
        let fragment = vec![distro("a", &["2", "1"])];
        write_tree(&root, &distro_list(&fragment), &fragment)?;
        let read = read_fragment(&root);
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(read?, fragment);
        Ok(())
    }

    #[test]
    fn unreadable_fragment_is_an_error() {
        let root = scratch("bad-fragment");
        let _ = std::fs::create_dir_all(root.join("v1"));
        let _ = std::fs::write(root.join("v1/index.json"), "not json");
        let read = read_fragment(&root);
        let _ = std::fs::remove_dir_all(&root);
        assert!(read.is_err());
    }

    #[test]
    fn previous_artifact_is_found_by_url_across_releases() -> Result<()> {
        let root = scratch("prev-artifact");
        let mut tree = vec![distro("a", &["2", "1"])];
        let artifact = api::Artifact {
            variant: Some("v".to_string()),
            default_variant: Some(true),
            arch: "x86_64".to_string(),
            format: api::ImageFormat::Qcow2,
            compression: api::Compression::None,
            firmware: Vec::new(),
            datasources: Vec::new(),
            ssh_key_injection: Some(true),
            size: Some(7),
            locations: vec![api::Location {
                url: "https://a.example/1.qcow2".to_string(),
                kind: Some(api::LocationKind::Primary),
                redirects_off_host: Some(false),
            }],
            integrity: Some(api::Integrity {
                digests: Vec::new(),
                signatures: Vec::new(),
                http: None,
            }),
        };
        tree[0].1[1].builds[0].artifacts.push(artifact);
        write_tree(&root, &distro_list(&tree), &tree)?;
        let loaded = api::tree::validate(&root).map_err(|p| anyhow::anyhow!(p.join("\n")));
        let _ = std::fs::remove_dir_all(&root);
        let loaded = loaded?;
        assert_eq!(
            previous_artifact(&loaded, "a", "https://a.example/1.qcow2").and_then(|a| a.size),
            Some(7)
        );
        assert!(previous_artifact(&loaded, "a", "https://a.example/none").is_none());
        assert!(previous_artifact(&loaded, "b", "https://a.example/1.qcow2").is_none());
        Ok(())
    }

    #[test]
    fn previous_tree_absent_is_none() -> Result<()> {
        assert!(load_previous(&scratch("prev-absent"))?.is_none());
        Ok(())
    }

    #[test]
    fn previous_tree_from_an_older_version_keeps_every_vendor() -> Result<()> {
        // Regression: a previous tree lacking a field added since must not
        // be treated as absent, or assembling with one fragment would drop
        // every other vendor.
        let root = scratch("prev-older");
        let previous = vec![distro("a", &["1"]), distro("b", &["1"])];
        write_tree(&root, &distro_list(&previous), &previous)?;
        let path = root.join("v1/distros/b/index.json");
        let mut v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        v.as_object_mut().map(|o| o.remove("homepage"));
        std::fs::write(&path, serde_json::to_string(&v)?)?;

        let loaded = load_previous(&root);
        let _ = std::fs::remove_dir_all(&root);
        let loaded = loaded?.ok_or_else(|| anyhow::anyhow!("previous tree not found"))?;
        let order = ["a", "b"].map(String::from);
        let merged = merge(&order, Some(&loaded), vec![distro("a", &["2"])])?;
        let ids: Vec<&str> = merged.iter().map(|(d, _)| d.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
        Ok(())
    }

    #[test]
    fn unreadable_previous_tree_is_an_error_not_absent() -> Result<()> {
        let root = scratch("prev-corrupt");
        std::fs::create_dir_all(root.join("v1"))?;
        std::fs::write(root.join("v1/index.json"), "not json")?;
        let loaded = load_previous(&root);
        let _ = std::fs::remove_dir_all(&root);
        assert!(loaded.is_err());
        Ok(())
    }

    #[test]
    fn previous_tree_missing_a_listed_distro_is_an_error() -> Result<()> {
        // Its vendor would otherwise vanish from the next assembly.
        let root = scratch("prev-missing-distro");
        let previous = vec![distro("a", &["1"]), distro("b", &["1"])];
        write_tree(&root, &distro_list(&previous), &previous)?;
        std::fs::remove_file(root.join("v1/distros/b/index.json"))?;
        let loaded = load_previous(&root);
        let _ = std::fs::remove_dir_all(&root);
        let e = loaded.err().map(|e| format!("{e:#}")).unwrap_or_default();
        assert!(e.contains("\"b\""), "{e}");
        Ok(())
    }

    #[test]
    fn previous_tree_without_a_distro_list_is_an_error() -> Result<()> {
        let root = scratch("prev-no-list");
        let previous = vec![distro("a", &["1"])];
        write_tree(&root, &distro_list(&previous), &previous)?;
        std::fs::remove_file(root.join("v1/index.json"))?;
        let loaded = load_previous(&root);
        let _ = std::fs::remove_dir_all(&root);
        assert!(loaded.is_err());
        Ok(())
    }

    #[test]
    fn merge_refuses_a_previous_vendor_with_missing_release_files() -> Result<()> {
        // A previous tree missing one of a vendor's release files must not
        // make that vendor silently disappear from the next assembly.
        let root = scratch("prev-incomplete");
        let previous = vec![distro("a", &["1"]), distro("b", &["2", "1"])];
        write_tree(&root, &distro_list(&previous), &previous)?;
        std::fs::remove_file(root.join("v1/distros/b/releases/1/index.json"))?;
        let loaded = load_previous(&root);
        let _ = std::fs::remove_dir_all(&root);
        let loaded = loaded?.ok_or_else(|| anyhow::anyhow!("previous tree not found"))?;
        let order = ["a", "b"].map(String::from);
        let merged = merge(&order, Some(&loaded), vec![distro("a", &["2"])]);
        assert!(merged.is_err(), "vendor b would have been dropped");
        Ok(())
    }

    #[test]
    fn write_tree_refuses_mismatched_build_lists() {
        let root = scratch("mismatch");
        let (d, mut lists) = distro("a", &["2", "1"]);
        lists.pop();
        let written = write_tree(
            &root,
            &distro_list(&[(d.clone(), lists.clone())]),
            &[(d, lists)],
        );
        let _ = std::fs::remove_dir_all(&root);
        assert!(written.is_err());
    }

    #[test]
    fn written_tree_validates_and_replaces_previous_output() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "cloud-image-index-generate-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        // Leftover from a previous run that must not survive.
        std::fs::create_dir_all(root.join("v1/distros/gone"))?;
        std::fs::write(root.join("v1/distros/gone/index.json"), "{}")?;

        let distro = Distro {
            id: "d".to_string(),
            name: Some("D".to_string()),
            os_family: OsFamily::Linux,
            homepage: Some("https://d.example/".to_string()),
            dev_channel: None,
            releases: vec![Release {
                id: "r".to_string(),
                version: Some("1".to_string()),
                title: Some("D 1".to_string()),
                aliases: vec![Alias::Latest],
                eol_date: None,
                osinfo: None,
            }],
        };
        let summary = DistroList {
            distros: vec![DistroSummary {
                id: "d".to_string(),
                name: Some("D".to_string()),
                os_family: OsFamily::Linux,
                homepage: Some("https://d.example/".to_string()),
            }],
        };
        write_tree(
            &root,
            &summary,
            &[(distro, vec![list(vec![build("b", at(1))])])],
        )?;
        let result = api::tree::validate(&root);
        let _ = std::fs::remove_dir_all(&root);
        let tree = result.map_err(|p| anyhow::anyhow!(p.join("\n")))?;
        assert!(tree.aliases.contains_key(&("d".to_string(), Alias::Latest)));
        assert!(!tree.distros.contains_key("gone"));
        Ok(())
    }
}
