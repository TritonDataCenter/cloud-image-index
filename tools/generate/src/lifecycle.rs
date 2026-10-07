// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Prune end-of-life releases using endoflife.date, one rule for every
//! vendor it covers.
//!
//! The index only offers releases worth installing. Some vendors say
//! when a release has ended in data the resolvers read; others publish
//! nothing machine-readable, and their resolvers list every release they
//! find. Either way, a release endoflife.date says has ended is not
//! offered. endoflife.date only removes releases: it never adds one, and
//! nothing it says is published (an `eol_date` comes from the vendor or
//! is left empty).
//!
//! The data is endoflife.date's (MIT licensed), read from
//! `https://endoflife.date/api/v1/products/<product>`.

use chrono::NaiveDate;
use resolvers::{Vendor, VersionEntry};
use serde::Deserialize;

/// The endoflife.date product for a vendor; `None` for vendors it does
/// not cover.
pub fn product(vendor: Vendor) -> Option<&'static str> {
    Some(match vendor {
        Vendor::Ubuntu => "ubuntu",
        Vendor::Debian => "debian",
        Vendor::Fedora => "fedora",
        Vendor::Alpine => "alpine-linux",
        Vendor::Opensuse => "opensuse",
        Vendor::Rocky => "rocky-linux",
        Vendor::Alma => "almalinux",
        Vendor::CentosStream => "centos-stream",
        Vendor::Oracle => "oracle-linux",
        Vendor::Freebsd => "freebsd",
        Vendor::Openbsd => "openbsd",
        Vendor::Arch | Vendor::Omnios | Vendor::Smartos | Vendor::Talos => return None,
    })
}

/// The URL of a product's lifecycle data.
pub fn product_url(product: &str) -> String {
    format!("https://endoflife.date/api/v1/products/{product}")
}

/// One release cycle as endoflife.date describes it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Cycle {
    /// The cycle's name, e.g. `9`, `24.04`, `15.1`.
    pub name: String,
    #[serde(rename = "isEol")]
    pub is_eol: bool,
    #[serde(rename = "eolFrom")]
    pub eol_from: Option<NaiveDate>,
}

#[derive(Deserialize)]
struct ProductDocument {
    result: ProductResult,
}

#[derive(Deserialize)]
struct ProductResult {
    releases: Vec<Cycle>,
}

/// The cycles in a product document.
pub fn parse_product(body: &str) -> anyhow::Result<Vec<Cycle>> {
    Ok(serde_json::from_str::<ProductDocument>(body)?
        .result
        .releases)
}

/// The cycle a version entry belongs to: its token (`9`, `15.1`), its
/// version (`24.04`), or that version's major (`13` for Debian's
/// `13.4`), in that order.
pub fn find_cycle<'a>(entry: &VersionEntry, cycles: &'a [Cycle]) -> Option<&'a Cycle> {
    let major = entry.version.split('.').next().unwrap_or_default();
    [entry.token.as_str(), entry.version.as_str(), major]
        .into_iter()
        .find_map(|key| cycles.iter().find(|c| c.name == key))
}

/// Whether a cycle has ended: endoflife.date says so, or its end date
/// has passed.
fn ended(cycle: &Cycle, today: NaiveDate) -> bool {
    cycle.is_eol || cycle.eol_from.is_some_and(|d| d <= today)
}

/// Mark every release that endoflife.date says has ended as
/// unsupported, so it is not offered. Returns notes on what was pruned
/// and on supported releases endoflife.date does not know (usually new
/// ones, which stay). Pre-releases, development channels and channels
/// such as `latest` are left alone: endoflife.date does not list them,
/// and a channel follows a current release.
pub fn prune(
    vendor: Vendor,
    entries: &mut [VersionEntry],
    cycles: &[Cycle],
    today: NaiveDate,
) -> Vec<String> {
    let mut notes = Vec::new();
    for entry in entries
        .iter_mut()
        .filter(|e| e.supported && e.dev.is_none() && !e.channel)
    {
        match find_cycle(entry, cycles) {
            None => notes.push(format!(
                "{vendor}: endoflife.date has no cycle for {:?}; offered as the vendor lists it",
                entry.token
            )),
            Some(cycle) if ended(cycle, today) => {
                entry.supported = false;
                notes.push(format!(
                    "{vendor}: {:?} has ended per endoflife.date; not offered",
                    entry.token
                ));
            }
            Some(_) => {}
        }
    }
    notes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap_or_else(|e| panic!("{e}"))
    }

    fn entry(token: &str, version: &str, supported: bool) -> VersionEntry {
        VersionEntry {
            token: token.to_string(),
            series: token.to_string(),
            version: version.to_string(),
            title: token.to_string(),
            eol_date: None,
            supported,
            lts: false,
            dev: None,
            channel: false,
        }
    }

    fn cycle(name: &str, is_eol: bool, eol_from: &str) -> Cycle {
        Cycle {
            name: name.to_string(),
            is_eol,
            eol_from: Some(date(eol_from)),
        }
    }

    const TODAY: &str = "2026-10-06";

    #[test]
    fn parses_the_v1_product_document() -> anyhow::Result<()> {
        let body = r#"{"schema_version":"1.2.1","result":{"name":"oracle-linux","releases":[
            {"name":"10","codename":null,"isEol":false,"eolFrom":"2035-06-30","isMaintained":true},
            {"name":"7","codename":null,"isEol":true,"eolFrom":"2024-12-31","isMaintained":true}
        ]}}"#;
        assert_eq!(
            parse_product(body)?,
            vec![
                cycle("10", false, "2035-06-30"),
                cycle("7", true, "2024-12-31")
            ]
        );
        Ok(())
    }

    #[test]
    fn matches_by_token_version_or_major() {
        let cycles = [
            cycle("24.04", false, "2029-05-31"),
            cycle("13", false, "2030-06-30"),
        ];
        let by_version = entry("noble", "24.04", true);
        let by_major = entry("trixie", "13.4", true);
        assert_eq!(
            find_cycle(&by_version, &cycles).map(|c| c.name.as_str()),
            Some("24.04")
        );
        assert_eq!(
            find_cycle(&by_major, &cycles).map(|c| c.name.as_str()),
            Some("13")
        );
        assert!(find_cycle(&entry("x", "99", true), &cycles).is_none());
    }

    #[test]
    fn a_release_endoflife_date_says_has_ended_is_no_longer_offered() {
        let mut entries = [entry("8", "8", true), entry("7", "7", true)];
        let cycles = [
            cycle("8", false, "2029-07-31"),
            cycle("7", true, "2024-12-31"),
        ];
        let notes = prune(Vendor::Oracle, &mut entries, &cycles, date(TODAY));
        assert!(entries[0].supported);
        assert!(!entries[1].supported);
        assert_eq!(notes.len(), 1, "{notes:?}");
    }

    #[test]
    fn a_past_end_date_counts_even_before_the_flag_flips() {
        let mut entries = [entry("15.0", "15.0", true)];
        let cycles = [cycle("15.0", false, "2026-09-30")];
        prune(Vendor::Freebsd, &mut entries, &cycles, date(TODAY));
        assert!(!entries[0].supported);
    }

    #[test]
    fn endoflife_date_prunes_even_where_the_vendor_still_says_supported() {
        // openSUSE's own feed still called 15.6 stable after its end.
        let mut entries = [entry("15.6", "15.6", true)];
        let cycles = [cycle("15.6", true, "2026-04-30")];
        prune(Vendor::Opensuse, &mut entries, &cycles, date(TODAY));
        assert!(!entries[0].supported);
    }

    #[test]
    fn nothing_from_endoflife_date_is_published() {
        let mut entries = [entry("9", "9", true)];
        let cycles = [cycle("9", false, "2032-05-31")];
        prune(Vendor::Rocky, &mut entries, &cycles, date(TODAY));
        assert_eq!(
            entries[0].eol_date, None,
            "end dates come only from vendors"
        );
    }

    #[test]
    fn it_never_adds_a_release_the_vendor_does_not_support() {
        let mut entries = [entry("43", "43", false)];
        let cycles = [cycle("43", false, "2026-12-09")];
        prune(Vendor::Fedora, &mut entries, &cycles, date(TODAY));
        assert!(!entries[0].supported);
    }

    #[test]
    fn pre_releases_channels_and_unknown_new_releases_are_left_as_they_are() {
        let mut beta = entry("45_Beta", "45 Beta", false);
        beta.dev = Some("beta".to_string());
        let mut latest = entry("latest", "latest", true);
        latest.channel = true;
        let mut entries = [beta, latest, entry("11", "11", true)];
        let notes = prune(Vendor::Rocky, &mut entries, &[], date(TODAY));
        assert!(entries[1].supported, "a channel is never end-of-life");
        assert!(entries[2].supported, "a release endoflife.date lacks stays");
        assert_eq!(
            notes.len(),
            1,
            "only the unknown release is noted: {notes:?}"
        );
    }

    #[test]
    fn eleven_vendors_are_on_endoflife_date() {
        let covered = Vendor::ALL
            .iter()
            .filter(|v| product(**v).is_some())
            .count();
        assert_eq!(covered, 11);
    }
}
