//! The `check-update` subcommand: ask GitHub for the newest release and
//! compare it against the running build.
//!
//! Two deliberate omissions.
//!
//! It is a subcommand rather than a check that runs alongside every query.
//! Every other subcommand talks to exactly one host, crt.sh, and a silent
//! call to a second one would add a network round trip — and a beacon — to a
//! tool people run from cron.
//!
//! There is no self-update counterpart. Fetching a binary and executing it is
//! the one operation where a compromised release channel gets code execution
//! for free, and the alternatives cost a user a single line: re-run the
//! install script, which verifies the release's SHA256SUMS before it replaces
//! anything, or rebuild from source.

use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::cli::OutputOpts;
use crate::output::{self, UpdateStatus};

/// The newest release GitHub considers current — never a draft, never a
/// prerelease, so the tag it names is always one with published archives.
const LATEST_RELEASE_API: &str =
    "https://api.github.com/repos/tiredithumans/crt-query/releases/latest";

/// A courtesy check, not a gate: give up rather than hang a terminal on a
/// network that is not going to answer.
const TIMEOUT_SECS: &str = "10";

const USER_AGENT: &str = concat!("crt-query/", env!("CARGO_PKG_VERSION"));

/// What to do about a newer release. Printed to stderr so the one-line
/// report on stdout stays the only thing a script has to parse.
const UPGRADE_HINT: &str = "\
Upgrade: re-run the install script — it resolves the latest release and \
verifies its SHA256SUMS.\n  \
Linux/macOS: curl -fsSL https://raw.githubusercontent.com/tiredithumans/crt-query/main/install.sh | sh\n  \
From source: cargo install --locked --git https://github.com/tiredithumans/crt-query --force";

/// The fields used out of GitHub's release object; serde ignores the rest.
#[derive(Deserialize)]
struct LatestRelease {
    tag_name: String,
    html_url: String,
}

/// A semantic version, split so that components compare as numbers.
struct Version {
    core: [u64; 3],
    pre: Option<String>,
}

impl Version {
    /// Parse `1.2.3`, `1.2.3-rc.1` or `1.2.3+build`. Anything else is `None`,
    /// which sends [`is_newer`] down its conservative path.
    fn parse(text: &str) -> Option<Self> {
        // Build metadata is explicitly not part of precedence in semver.
        let text = text.split('+').next()?;
        let (core_text, pre) = match text.split_once('-') {
            Some((core, pre)) => (core, Some(pre.to_string())),
            None => (text, None),
        };
        let mut core = [0_u64; 3];
        let mut fields = core_text.split('.');
        for slot in &mut core {
            *slot = fields.next()?.trim().parse().ok()?;
        }
        if fields.next().is_some() {
            return None;
        }
        Some(Self { core, pre })
    }

    /// Precedence key. A release outranks any prerelease of the same core
    /// version; two prereleases of one core compare as text, which is enough
    /// here because `releases/latest` never points at a prerelease.
    fn rank(&self) -> ([u64; 3], bool, &str) {
        (
            self.core,
            self.pre.is_none(),
            self.pre.as_deref().unwrap_or(""),
        )
    }
}

/// Whether `latest` is a strictly newer release than `current`.
///
/// Component-wise numeric comparison, because 0.10.0 is newer than 0.9.0 even
/// though it sorts earlier as text. If either side does not parse, any
/// difference is reported as an update rather than silently claiming the build
/// is current.
fn is_newer(latest: &str, current: &str) -> bool {
    match (Version::parse(latest), Version::parse(current)) {
        (Some(l), Some(c)) => l.rank() > c.rank(),
        _ => latest != current,
    }
}

/// Fetch the newest release through the system `curl`.
///
/// Shelling out rather than linking an HTTP client: TLS plus an async client
/// is a large addition to a dependency tree this project audits on every PR,
/// and it would be pulled in for one opt-in subcommand that the tool's actual
/// job never touches.
fn fetch_latest_release() -> Result<LatestRelease> {
    let output = Command::new("curl")
        .args([
            "--silent",
            "--show-error",
            "--location",
            "--fail",
            "--max-time",
            TIMEOUT_SECS,
            "--user-agent",
            USER_AGENT,
            "--header",
            "Accept: application/vnd.github+json",
            LATEST_RELEASE_API,
        ])
        .output()
        .context(
            "could not run curl, which check-update needs to reach GitHub; \
             install curl, or open \
             https://github.com/tiredithumans/crt-query/releases/latest",
        )?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        let detail = if detail.is_empty() {
            format!("curl exited with {}", output.status)
        } else {
            detail.to_string()
        };
        // GitHub rate-limits unauthenticated callers per IP, which is the one
        // failure a user can wait out rather than debug.
        bail!(
            "could not reach the GitHub releases API ({detail}); \
             it rate-limits unauthenticated requests, so retry later or open \
             https://github.com/tiredithumans/crt-query/releases/latest"
        );
    }

    serde_json::from_slice(&output.stdout)
        .context("the GitHub releases API returned a response this build could not parse")
}

/// Compare the running build against the newest release.
fn check() -> Result<UpdateStatus> {
    let release = fetch_latest_release()?;
    // Tags carry a `v` prefix; Cargo versions do not.
    let latest = release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name);
    let current = env!("CARGO_PKG_VERSION");
    Ok(UpdateStatus {
        update_available: is_newer(latest, current),
        current: current.to_string(),
        latest: latest.to_string(),
        release_url: release.html_url,
    })
}

/// Run `check-update`: report the comparison, and say how to act on it.
pub fn run_check_update(out: &OutputOpts) -> Result<()> {
    let status = check()?;
    output::emit_update_status(&status, out)?;
    if status.update_available {
        eprintln!("{UPGRADE_HINT}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_equal_version_is_not_an_update() {
        assert!(!is_newer("0.1.0", "0.1.0"));
    }

    #[test]
    fn components_compare_as_numbers_not_text() {
        assert!(
            is_newer("0.10.0", "0.9.0"),
            "0.10.0 sorts before 0.9.0 as text but is the newer release"
        );
        assert!(!is_newer("0.9.0", "0.10.0"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(is_newer("0.1.10", "0.1.9"));
    }

    #[test]
    fn an_older_release_than_the_running_build_is_not_an_update() {
        assert!(!is_newer("0.1.0", "0.2.0"));
    }

    #[test]
    fn a_release_beats_a_prerelease_of_the_same_version() {
        assert!(is_newer("0.2.0", "0.2.0-rc.1"));
        assert!(!is_newer("0.2.0-rc.1", "0.2.0"));
    }

    #[test]
    fn build_metadata_does_not_affect_precedence() {
        assert!(!is_newer("0.1.0+abc", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0+abc"));
    }

    #[test]
    fn an_unparseable_version_reports_any_difference() {
        // Better to send someone to the release page than to assert that a
        // version this build cannot read is up to date.
        assert!(is_newer("not-a-version", "0.1.0"));
        assert!(!is_newer("not-a-version", "not-a-version"));
        assert!(!is_newer("0.1", "0.1"));
        assert!(is_newer("0.1.0.1", "0.1.0"));
    }

    #[test]
    fn the_release_json_subset_parses() {
        let release: LatestRelease = serde_json::from_str(
            r#"{"tag_name":"v9.9.9","html_url":"https://example.invalid/r/v9.9.9",
                "name":"v9.9.9","draft":false,"assets":[{"name":"x"}]}"#,
        )
        .expect("extra fields are ignored");
        assert_eq!(release.tag_name, "v9.9.9");
        assert_eq!(release.html_url, "https://example.invalid/r/v9.9.9");
    }

    #[test]
    fn the_upgrade_hint_names_a_verifying_path() {
        assert!(UPGRADE_HINT.contains("install.sh"));
        assert!(UPGRADE_HINT.contains("SHA256SUMS"));
        // `cargo install` re-resolves versions unless told not to, so without
        // this the from-source route builds a dependency set no gate has seen
        // — while every justfile recipe and release.yml pass --locked.
        assert!(
            UPGRADE_HINT.contains("cargo install --locked"),
            "the from-source hint must pin the lockfile:\n{UPGRADE_HINT}"
        );
    }
}
