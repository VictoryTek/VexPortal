//! Compare the compiled-in catalog against the justfile actually installed on this
//! machine.
//!
//! This is the check that catches a vexos-nix change VexPortal has not caught up with.
//! It needs a real justfile and a real `just`, neither of which exists inside the Nix
//! build sandbox, so it reports and skips when they are missing — run it on a VexOS
//! host (`cargo test -p vexportal-catalog`) for it to mean anything.

use std::path::Path;
use std::process::Command;
use vexportal_catalog::drift::{compare, JustDump};
use vexportal_catalog::Catalog;

/// Where a built VexOS host keeps the justfile the daemon will actually run.
const JUSTFILE: &str = "/etc/nixos/justfile";

#[test]
fn catalog_matches_the_installed_justfile() {
    if !Path::new(JUSTFILE).exists() {
        eprintln!("skipping: {JUSTFILE} not present (not a built VexOS host)");
        return;
    }

    let output = match Command::new("just")
        .args([
            "--justfile",
            JUSTFILE,
            "--working-directory",
            "/etc/nixos",
            "--dump",
            "--dump-format",
            "json",
        ])
        .output()
    {
        Ok(output) => output,
        Err(e) => {
            eprintln!("skipping: could not run `just`: {e}");
            return;
        }
    };

    assert!(
        output.status.success(),
        "`just --dump` failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let dump = JustDump::parse(&String::from_utf8_lossy(&output.stdout))
        .expect("`just --dump --dump-format json` should be parseable");
    let catalog = Catalog::load().expect("catalog should load");

    let drift = compare(&catalog, &dump);
    let (defects, host_behind): (Vec<_>, Vec<_>) =
        drift.iter().partition(|d| d.is_catalog_defect());

    // `/etc/nixos/justfile` is a copy taken by the last rebuild, so a host that has
    // not rebuilt since vexos-nix gained a recipe will be missing it. That is the
    // host's state, not a defect in the catalog — report it and carry on.
    if !host_behind.is_empty() {
        eprintln!(
            "note: this host's justfile predates {} catalog {}:\n{}",
            host_behind.len(),
            if host_behind.len() == 1 {
                "entry"
            } else {
                "entries"
            },
            host_behind
                .iter()
                .map(|d| format!("  - {}", d.describe()))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    assert!(
        defects.is_empty(),
        "the catalog has drifted from {JUSTFILE}:\n{}",
        defects
            .iter()
            .map(|d| format!("  - {}", d.describe()))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn dynamic_choice_sources_resolve_against_the_installed_justfile() {
    if !Path::new(JUSTFILE).exists() {
        eprintln!("skipping: {JUSTFILE} not present");
        return;
    }
    let Ok(output) = Command::new("just")
        .args([
            "--justfile",
            JUSTFILE,
            "--working-directory",
            "/etc/nixos",
            "--dump",
            "--dump-format",
            "json",
        ])
        .output()
    else {
        eprintln!("skipping: could not run `just`");
        return;
    };
    let dump = JustDump::parse(&String::from_utf8_lossy(&output.stdout)).unwrap();

    // Every dropdown fed by a justfile variable must actually find that variable,
    // otherwise the GUI silently renders an empty list.
    for variable in ["_feature_names", "_server_service_names"] {
        assert!(
            !dump.list_variable(variable).is_empty(),
            "`{variable}` is empty or missing from the justfile, so its dropdown would be blank"
        );
    }
}
