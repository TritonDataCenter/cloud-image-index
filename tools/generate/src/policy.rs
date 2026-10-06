// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Generic rules for turning a vendor's version list into releases and
//! aliases. Everything vendor-specific is reported by the resolvers as
//! labels on each version entry (`lts`, `dev`, `channel`) and, for
//! channels, the release the channel resolved to (`facts.release`).
//!
//! - An entry becomes a release if the vendor supports it, or if it is
//!   a development channel or pre-release. Its token must be valid.
//! - A channel is named after the release it resolved to; any other
//!   entry keeps its token.
//! - Anything the rules cannot place (a wanted entry with an invalid
//!   token, a channel that did not say which release it resolved to, a
//!   release name that is not a valid token) is an error, so the vendor
//!   fails and keeps its previous files rather than losing a release,
//!   and perhaps an alias, without anyone noticing.
//! - `latest` is the newest release that is not a dev entry; `lts` the
//!   newest the vendor labels LTS; `dev` the newest dev entry, whose
//!   vendor name becomes the distro's `dev_channel`. Entries are newest
//!   first (each resolver tests that it lists them so).

use anyhow::{Context, Result};
use api::Alias;
use resolvers::{ResolvedImage, VersionEntry, validate_version_token};

/// Whether a version-list entry becomes a release: the vendor must
/// still support it, unless it is a development channel or
/// pre-release. An entry that would be included but whose token the
/// resolvers do not accept is an error.
pub fn include_entry(entry: &VersionEntry) -> Result<bool> {
    if !entry.supported && entry.dev.is_none() {
        return Ok(false);
    }
    validate_version_token(&entry.token)
        .with_context(|| format!("version {:?} has an invalid token", entry.token))?;
    Ok(true)
}

/// A list entry together with what it resolved to.
pub struct Candidate<'a> {
    pub entry: &'a VersionEntry,
    pub image: &'a ResolvedImage,
}

/// One release of the generated index, before files are written.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedRelease {
    pub id: String,
    pub version: String,
    pub title: String,
    pub aliases: Vec<Alias>,
    /// The vendor's end-of-support date for the release, if given.
    pub eol_date: Option<chrono::NaiveDate>,
    /// Index into the candidates this release's build comes from.
    pub candidate: usize,
    /// Labels gathered from every candidate that named this release.
    lts: bool,
    dev: Option<String>,
    not_dev: bool,
}

/// The releases of one distro, with their aliases.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Plan {
    pub releases: Vec<PlannedRelease>,
    /// The vendor's name for the channel the `dev` alias follows.
    pub dev_channel: Option<String>,
}

/// Name every candidate's release and assign aliases, merging
/// candidates that resolved to the same release (e.g. two channels on
/// one release). Candidates are newest first; `distro_name` titles
/// channel releases (e.g. `OmniOS r151058`).
pub fn plan(distro_name: &str, candidates: &[Candidate<'_>]) -> Result<Plan> {
    let mut plan = Plan::default();
    for (i, c) in candidates.iter().enumerate() {
        let (id, version, title) = if c.entry.channel {
            let Some(release) = c.image.facts.release.clone() else {
                anyhow::bail!(
                    "channel {:?} did not say which release it resolved to",
                    c.entry.token
                );
            };
            validate_version_token(&release).with_context(|| {
                format!(
                    "channel {:?} resolved to release {release:?}, not a valid name",
                    c.entry.token
                )
            })?;
            let title = format!("{distro_name} {release}");
            (release.clone(), release, title)
        } else {
            (
                c.entry.token.clone(),
                c.entry.version.clone(),
                c.entry.title.clone(),
            )
        };
        let p = match plan.releases.iter().position(|p| p.id == id) {
            Some(existing) => &mut plan.releases[existing],
            None => {
                plan.releases.push(PlannedRelease {
                    id,
                    version,
                    title,
                    aliases: Vec::new(),
                    eol_date: c.entry.eol_date,
                    candidate: i,
                    lts: false,
                    dev: None,
                    not_dev: false,
                });
                let last = plan.releases.len() - 1;
                &mut plan.releases[last]
            }
        };
        p.lts |= c.entry.lts;
        match &c.entry.dev {
            Some(name) => p.dev = p.dev.clone().or(Some(name.clone())),
            None => p.not_dev = true,
        }
    }

    if let Some(p) = plan.releases.iter_mut().find(|p| p.not_dev) {
        p.aliases.push(Alias::Latest);
    }
    if let Some(p) = plan.releases.iter_mut().find(|p| p.lts) {
        p.aliases.push(Alias::Lts);
    }
    if let Some(p) = plan.releases.iter_mut().find(|p| p.dev.is_some()) {
        p.aliases.push(Alias::Dev);
        plan.dev_channel = p.dev.clone();
    }
    for p in &mut plan.releases {
        p.aliases.sort();
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use resolvers::SourceFormat;
    use resolvers::verify::Sha256Pinned;
    use url::Url;

    fn entry(token: &str, supported: bool) -> VersionEntry {
        VersionEntry {
            token: token.to_string(),
            series: token.to_string(),
            version: token.to_string(),
            title: format!("Title {token}"),
            eol_date: None,
            supported,
            lts: false,
            dev: None,
            channel: false,
        }
    }

    fn lts(mut e: VersionEntry) -> VersionEntry {
        e.lts = true;
        e
    }

    fn dev(mut e: VersionEntry, name: &str) -> VersionEntry {
        e.dev = Some(name.to_string());
        e
    }

    fn channel(mut e: VersionEntry) -> VersionEntry {
        e.channel = true;
        e
    }

    fn image(release: Option<&str>) -> ResolvedImage {
        let u = Url::parse("https://v.example/img").unwrap_or_else(|e| panic!("{e}"));
        let mut image = ResolvedImage {
            url: u.clone(),
            format: SourceFormat::Qcow2,
            os: "linux".to_string(),
            series: "s".to_string(),
            version: "1".to_string(),
            description: String::new(),
            homepage: u,
            ssh_key: true,
            verifier: Box::new(Sha256Pinned::new("abc".to_string())),
            expected_sha256: Some("abc".to_string()),
            facts: Default::default(),
        };
        image.facts.release = release.map(str::to_string);
        image
    }

    /// Plan `entries`, each resolving to an image whose channel release
    /// name is the matching element of `releases`.
    fn plan_of(entries: &[VersionEntry], releases: &[Option<&str>]) -> Plan {
        let images: Vec<ResolvedImage> = releases.iter().map(|r| image(*r)).collect();
        let c: Vec<_> = entries
            .iter()
            .zip(&images)
            .map(|(entry, image)| Candidate { entry, image })
            .collect();
        plan("Distro", &c).unwrap_or_else(|e| panic!("{e:#}"))
    }

    fn try_plan(entries: &[VersionEntry], releases: &[Option<&str>]) -> anyhow::Result<Plan> {
        let images: Vec<ResolvedImage> = releases.iter().map(|r| image(*r)).collect();
        let c: Vec<_> = entries
            .iter()
            .zip(&images)
            .map(|(entry, image)| Candidate { entry, image })
            .collect();
        plan("Distro", &c)
    }

    fn summary(plan: &Plan) -> Vec<(String, Vec<Alias>)> {
        plan.releases
            .iter()
            .map(|p| (p.id.clone(), p.aliases.clone()))
            .collect()
    }

    #[test]
    fn include_entry_takes_supported_releases_and_dev_channels() -> anyhow::Result<()> {
        assert!(include_entry(&entry("44", true))?);
        assert!(!include_entry(&entry("questing", false))?, "unsupported");
        assert!(
            !include_entry(&entry("45 Beta", false))?,
            "unsupported, any token"
        );
        assert!(include_entry(&dev(entry("bloody", false), "bloody"))?);
        Ok(())
    }

    #[test]
    fn a_wanted_entry_with_an_invalid_token_fails_rather_than_vanishing() {
        assert!(include_entry(&entry("45 Beta", true)).is_err());
        assert!(include_entry(&dev(entry("../x", false), "beta")).is_err());
    }

    #[test]
    fn newest_release_is_latest_and_newest_lts_is_lts() {
        let p = plan_of(
            &[lts(entry("resolute", true)), lts(entry("noble", true))],
            &[None, None],
        );
        assert_eq!(
            summary(&p),
            [
                ("resolute".to_string(), vec![Alias::Latest, Alias::Lts]),
                ("noble".to_string(), vec![]),
            ]
        );
    }

    #[test]
    fn no_lts_label_means_no_lts_alias() {
        let p = plan_of(&[entry("10", true), entry("9", true)], &[None, None]);
        assert_eq!(
            summary(&p),
            [
                ("10".to_string(), vec![Alias::Latest]),
                ("9".to_string(), vec![]),
            ]
        );
    }

    #[test]
    fn a_pre_release_is_dev_and_never_latest() {
        let p = plan_of(
            &[dev(entry("45_Beta", false), "beta"), entry("44", true)],
            &[None, None],
        );
        assert_eq!(
            summary(&p),
            [
                ("45_Beta".to_string(), vec![Alias::Dev]),
                ("44".to_string(), vec![Alias::Latest]),
            ]
        );
        assert_eq!(p.dev_channel.as_deref(), Some("beta"));
    }

    #[test]
    fn channels_are_named_after_the_release_they_resolved_to() {
        let p = plan_of(
            &[
                channel(entry("stable", true)),
                lts(channel(entry("lts", true))),
                dev(channel(entry("bloody", false)), "bloody"),
            ],
            &[Some("r151058"), Some("r151054r"), Some("20260907")],
        );
        assert_eq!(
            summary(&p),
            [
                ("r151058".to_string(), vec![Alias::Latest]),
                ("r151054r".to_string(), vec![Alias::Lts]),
                ("20260907".to_string(), vec![Alias::Dev]),
            ]
        );
        assert_eq!(p.releases[0].title, "Distro r151058");
        assert_eq!(p.releases[0].version, "r151058");
        assert_eq!(p.dev_channel.as_deref(), Some("bloody"));
    }

    #[test]
    fn channels_resolving_to_one_release_merge() {
        let p = plan_of(
            &[
                channel(entry("stable", true)),
                lts(channel(entry("lts", true))),
            ],
            &[Some("r151054r"), Some("r151054r")],
        );
        assert_eq!(
            summary(&p),
            [("r151054r".to_string(), vec![Alias::Latest, Alias::Lts])]
        );
    }

    #[test]
    fn a_channel_without_a_release_name_fails_rather_than_guessing() {
        let p = try_plan(
            &[channel(entry("latest", true)), entry("1", true)],
            &[None, None],
        );
        let e = p.err().map(|e| format!("{e:#}")).unwrap_or_default();
        assert!(e.contains("\"latest\""), "{e}");
    }

    #[test]
    fn a_channel_resolving_to_an_invalid_release_name_fails() {
        let p = try_plan(&[channel(entry("stable", true))], &[Some("r1 beta")]);
        assert!(p.is_err());
    }

    #[test]
    fn release_takes_the_vendor_eol_date() {
        let mut e = entry("10", true);
        e.eol_date = chrono::NaiveDate::from_ymd_opt(2035, 5, 31);
        let p = plan_of(std::slice::from_ref(&e), &[None]);
        assert_eq!(p.releases[0].eol_date, e.eol_date);
    }
}
