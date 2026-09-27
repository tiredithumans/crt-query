//! Checks that the README states the same facts as the files it describes.
//!
//! Every stale claim this repository has had to correct was a number or a list
//! written out by hand in one place and changed in another. These tests read
//! both sides and compare them, so the drift fails CI instead of reaching a
//! reader.
//!
//! Offline and dependency-free: both files are embedded at compile time.

const README: &str = include_str!("../README.md");
const MANIFEST: &str = include_str!("../Cargo.toml");

/// The `rust-version` Cargo.toml declares, e.g. `1.98`.
///
/// Read by hand rather than through a TOML parser: the line has one fixed shape
/// in a manifest this repository controls, and a test that pins a documented
/// number should not need more machinery than the number.
fn declared_msrv() -> &'static str {
    MANIFEST
        .lines()
        .find_map(|line| {
            line.strip_prefix("rust-version = \"")
                .and_then(|rest| rest.strip_suffix('"'))
        })
        .expect("Cargo.toml declares a rust-version")
}

/// The badge at the top of the README hardcodes the MSRV, and nothing else
/// reads it. Bumping `rust-version` left it advertising the old one.
///
/// Deliberately says nothing about rust-toolchain.toml: the pinned toolchain
/// may move ahead of the MSRV, and the badge describes the MSRV.
#[test]
fn the_readme_badge_names_the_declared_msrv() {
    let msrv = declared_msrv();
    let badge = format!("img.shields.io/badge/rust-{msrv}-");
    assert!(
        README.contains(&badge),
        "the README's Rust badge does not say {msrv}, the rust-version in \
         Cargo.toml; update the shields.io badge URL at the top of README.md"
    );
}

/// The Build section states the same minimum in prose.
#[test]
fn the_readme_build_section_names_the_declared_msrv() {
    let msrv = declared_msrv();
    let claim = format!("Requires Rust {msrv}+");
    assert!(
        README.contains(&claim),
        "the README's Build section does not say \"{claim}\", matching the \
         rust-version in Cargo.toml; update it"
    );
}
