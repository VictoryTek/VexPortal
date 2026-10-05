//! Compare the compiled-in catalog against a recorded vexos-nix justfile.
//!
//! `drift_against_justfile.rs` needs a built VexOS host, so on a development machine
//! and in the Nix build sandbox it skips. This test runs everywhere, against
//! `fixtures/vexos-nix-justfile.json`: the fields of `just --dump --dump-format json`
//! that VexPortal reads, taken from the vexos-nix justfile the catalog was last synced
//! with. Refresh the fixture whenever the catalog is synced to a newer justfile:
//!
//!   just --justfile ../vexos-nix/justfile --dump --dump-format json
//!
//! trimmed to `assignments.*.value` and each recipe's `private`, `doc` and
//! `parameters` (`name`, `default`, `kind`).

use vexportal_catalog::drift::{compare, JustDump};
use vexportal_catalog::Catalog;

const FIXTURE: &str = include_str!("fixtures/vexos-nix-justfile.json");

fn dump() -> JustDump {
    JustDump::parse(FIXTURE).expect("the fixture should parse as a justfile dump")
}

#[test]
fn catalog_matches_the_recorded_justfile() {
    let drift = compare(&Catalog::load().unwrap(), &dump());
    assert!(
        drift.is_empty(),
        "the catalog has drifted from the recorded justfile:\n{}",
        drift
            .iter()
            .map(|d| format!("  - {}", d.describe()))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn every_feature_switch_has_catalog_metadata() {
    let catalog = Catalog::load().unwrap();
    for name in dump().list_variable("_feature_names") {
        assert!(
            catalog.features.iter().any(|f| f.name == name),
            "`{name}` is in `_feature_names` but has no [[feature]] entry"
        );
    }
}

#[test]
fn the_service_catalog_is_grouped_and_described() {
    let services = dump().service_catalog();
    assert!(!services.is_empty());
    assert!(services
        .iter()
        .all(|s| !s.group.is_empty() && !s.name.is_empty()));
    let plex = services
        .iter()
        .find(|s| s.name == "plex")
        .expect("plex is a service");
    assert!(!plex.description.is_empty());
}
