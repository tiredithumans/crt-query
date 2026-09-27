//! The `check-update` subcommand: ask GitHub for the newest release and
//! compare it against the running build.
//!
//! The answer is read from a redirect, not from the REST API. A request for
//! `github.com/<repo>/releases/latest` is answered with a 302 whose `Location`
//! is `/releases/tag/<tag>`, so the tag is in a header and the body can be
//! thrown away unread. This used to ask
//! `api.github.com/repos/<repo>/releases/latest` instead, and that endpoint's
//! per-IP limit on unauthenticated requests was the one failure its own error
//! message had to explain — to someone behind a shared address, a limit other
//! people's traffic had already spent. The install scripts already avoided
//! the API for exactly that reason, downloading through
//! `/releases/latest/download/<asset>`; this reads the redirect one level up.
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

#[cfg(any(windows, test))]
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::cli::OutputOpts;
use crate::output::{self, UpdateStatus};

/// This repository's releases page, as a macro rather than a `const` so the
/// URLs below can be built from it with `concat!`, which takes literals only.
/// The repository is named once, so a rename or a transfer cannot update the
/// URL that is fetched and leave behind the prefix its answer is checked
/// against — a mismatch that would fail every check with a message blaming
/// GitHub.
macro_rules! releases_page {
    () => {
        "https://github.com/tiredithumans/crt-query/releases"
    };
}

/// Where a user can see the newest release for themselves, which is what every
/// error in this module offers when the check cannot say.
const RELEASES_PAGE: &str = releases_page!();

/// Redirects to the release GitHub considers current — never a draft, never a
/// prerelease, so the tag it names is always one with published archives.
const LATEST_RELEASE_PAGE: &str = concat!(releases_page!(), "/latest");

/// What the redirect from [`LATEST_RELEASE_PAGE`] starts with when it names a
/// release. Everything after it is the tag.
const RELEASE_TAG_PREFIX: &str = concat!(releases_page!(), "/tag/");

/// A courtesy check, not a gate: give up rather than hang a terminal on a
/// network that is not going to answer.
const TIMEOUT_SECS: &str = "10";

const USER_AGENT: &str = concat!("crt-query/", env!("CARGO_PKG_VERSION"));

/// Where curl writes the redirect's body, which nothing reads. It has to go
/// somewhere other than stdout, which carries the `--write-out` answer, so it
/// goes to the null device — spelt differently on each platform.
#[cfg(windows)]
const NULL_DEVICE: &str = "NUL";
#[cfg(not(windows))]
const NULL_DEVICE: &str = "/dev/null";

/// The name curl is run by wherever a full path is not known, left for the
/// platform's own search to resolve.
const CURL: &str = "curl";

/// The prebuilt install routes available on the platform this binary was built
/// for, ordered as the README's install table orders them.
///
/// Only this platform's. A Windows build used to print the `install.sh` line
/// and nothing else: a shell pipeline the user has no `sh` for, naming a script
/// that refuses to run on Windows anyway and could not install the binary they
/// are holding. Listing every platform instead would put lines that do not
/// apply in front of the one that does, on a message whose whole job is to be
/// actionable.
///
/// Homebrew is listed first where it runs — macOS and Linux both — because a
/// Homebrew install is upgraded with `brew upgrade`, and re-running
/// `install.sh` would instead drop a second, unmanaged copy in
/// `/usr/local/bin` for Homebrew's own to shadow or be shadowed by. It does not
/// appear on Windows, where Homebrew does not run.
///
/// Windows on ARM needs no separate entry: the same `install.ps1` line installs
/// the native ARM64 build, and falls back to the x86-64 one, which Windows runs
/// under emulation, only for a release that predates the native build.
///
/// Labels are padded so every command starts in the same column as the
/// `From source:` line below, which is the only thing making a three-line block
/// scannable.
#[cfg(windows)]
const INSTALL_ROUTES: &[&str] = &[
    "Windows:     irm https://raw.githubusercontent.com/tiredithumans/crt-query/main/install.ps1 | iex",
];

#[cfg(not(windows))]
const INSTALL_ROUTES: &[&str] = &[
    "Homebrew:    brew upgrade crt-query",
    "Linux/macOS: curl -fsSL https://raw.githubusercontent.com/tiredithumans/crt-query/main/install.sh | sh",
];

/// What to do about a newer release. Printed to stderr so the one-line
/// report on stdout stays the only thing a script has to parse.
///
/// A function rather than a `const` because [`INSTALL_ROUTES`] varies by target
/// and `concat!` takes literals only. Writing the surrounding prose out twice
/// under `cfg` would be the cheaper trick and the worse one — two copies of a
/// sentence that has to stay in step is exactly the shape every stale claim in
/// this repo has taken.
///
/// "re-run whatever you installed with", not "re-run the install script": with
/// Homebrew on the list the older wording pointed a `brew` user at the one
/// route that would go wrong for them. The verification claim is scoped to the
/// prebuilt routes — Homebrew checks the digest the formula carries, which
/// `just homebrew-formula` copies out of the release's `SHA256SUMS`, and both
/// scripts check that file directly. Building from source does none of this,
/// which is why it sits outside the sentence.
fn upgrade_hint() -> String {
    let routes = INSTALL_ROUTES.join("\n  ");
    format!(
        "Upgrade: re-run whatever you installed with — every prebuilt route checks \
         the download against a checksum from the release's SHA256SUMS.\n  \
         {routes}\n  \
         From source: cargo install --locked --git \
         https://github.com/tiredithumans/crt-query --force"
    )
}

/// The newest release, as the [`LATEST_RELEASE_PAGE`] redirect names it.
#[derive(Debug, PartialEq)]
struct LatestRelease {
    /// The tag exactly as GitHub spells it, `v` prefix and all.
    tag: String,
    /// The release's own page: the redirect itself, which is what a browser
    /// following [`LATEST_RELEASE_PAGE`] would land on.
    url: String,
}

impl LatestRelease {
    /// Read the release out of where [`LATEST_RELEASE_PAGE`] redirected, or
    /// say why that is not a release.
    ///
    /// The redirect has to be `/releases/tag/<tag>` on github.com for this
    /// repository, matched as one exact prefix rather than parsed as a URL,
    /// because anything else is not an answer to the question asked. A
    /// repository with no published release redirects somewhere else, such as
    /// its releases list, and a request that was never redirected at all has
    /// no `Location` to read. Taking whatever follows the last `/` in those
    /// would report `releases` or `latest` as the newest version — and since
    /// [`is_newer`] reports any version it cannot parse as an update, that
    /// would be a bogus "update available" rather than an error.
    ///
    /// The tag is held to the characters a version tag uses. That rules out a
    /// second path segment, a query or a fragment, all of which would mean the
    /// prefix matched something other than a tag; and since the tag and the URL
    /// are printed as they are, it also keeps anything a terminal would act on
    /// out of the one line a human reads.
    fn from_redirect(redirect: &str) -> Result<Self> {
        let redirect = redirect.trim();
        let Some(tag) = redirect.strip_prefix(RELEASE_TAG_PREFIX) else {
            let what = if redirect.is_empty() {
                format!("{LATEST_RELEASE_PAGE} did not redirect")
            } else {
                // Debug formatting quotes the URL and escapes any control
                // characters in it, which matters because it came off the
                // network and is about to be printed to a terminal.
                format!("{LATEST_RELEASE_PAGE} redirected to {redirect:?}, not to a release")
            };
            bail!("GitHub did not name the newest release: {what}; see {RELEASES_PAGE}");
        };
        let is_tag_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+');
        if tag.is_empty() || !tag.chars().all(is_tag_char) {
            bail!(
                "GitHub redirected to {redirect:?}, which does not name a release tag \
                 this build can read; see {RELEASES_PAGE}"
            );
        }
        Ok(Self {
            tag: tag.to_string(),
            url: redirect.to_string(),
        })
    }
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

/// Which curl to run on Windows, given the value of `%SystemRoot%` and a way
/// to ask whether a file exists.
///
/// A bare `curl` is not resolved through `PATH` on Windows. Rust's standard
/// library does its own search, and looks in the directory holding
/// `crt-query.exe` before `System32` and before `PATH`, so a `curl.exe`
/// unpacked next to `crt-query.exe` — from the same download folder, say — is
/// the one that runs. `System32` has had a `curl.exe` of its own since
/// Windows 10 1803, and naming it by its full path skips the search entirely.
///
/// `SystemRoot` has to be an absolute drive path, `C:\` or `C:/` onwards,
/// before it is trusted. A relative value would be resolved against the
/// current directory, which is the planting problem again by another route;
/// a drive-relative (`C:Windows`) or root-relative (`\Windows`) one depends on
/// the current directory or drive too; and a UNC share would have this reach
/// across the network just to decide which program to start. The rule is
/// spelt out by hand rather than left to [`Path::is_absolute`], which answers
/// for the host it runs on, while the tests check the Windows answer on every
/// host.
///
/// When there is no such file, or no usable `SystemRoot` — Windows before
/// 1803, or an environment that has lost the variable — this falls back to
/// the bare name and the search described above. That keeps the subcommand
/// working where it worked before, and leaves those systems exactly as exposed
/// as every system used to be, which `SECURITY.md` says in so many words.
#[cfg(any(windows, test))]
fn windows_curl(system_root: Option<&str>, exists: impl Fn(&Path) -> bool) -> String {
    let Some(root) = system_root.filter(|root| is_windows_drive_absolute(root)) else {
        return CURL.to_string();
    };
    let candidate = format!("{}\\System32\\curl.exe", root.trim_end_matches(['\\', '/']));
    if exists(Path::new(&candidate)) {
        candidate
    } else {
        CURL.to_string()
    }
}

/// Whether `path` is absolute by Windows rules and rooted at a drive letter:
/// `C:\…` or `C:/…`, on whatever host this runs on.
#[cfg(any(windows, test))]
fn is_windows_drive_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/')
}

/// The curl this build runs: see [`windows_curl`] for why Windows names one by
/// its full path. Everywhere else the bare name, resolved through `PATH` like
/// any other command.
///
/// A `SystemRoot` that is not valid Unicode is treated as unset. That costs a
/// system with one nothing but the fallback, and there is no such system in
/// practice: the variable is set by Windows itself.
fn curl_program() -> String {
    #[cfg(windows)]
    {
        windows_curl(std::env::var("SystemRoot").ok().as_deref(), Path::is_file)
    }
    #[cfg(not(windows))]
    {
        CURL.to_string()
    }
}

/// Ask GitHub which release is newest, through the system `curl`.
///
/// Shelling out rather than linking an HTTP client: TLS plus an async client
/// is a large addition to a dependency tree this project audits on every PR,
/// and it would be pulled in for one opt-in subcommand that the tool's actual
/// job never touches.
///
/// The answer is the redirect itself, so curl is told not to follow it and to
/// print where it pointed, sending the body to [`NULL_DEVICE`]. `--no-location`
/// is spelt out even though not following is the default: curl reads
/// `~/.curlrc` before its arguments, and a `location` line there — a common
/// enough convenience — would otherwise follow the redirect to the release
/// page, which answers `200` with no redirect of its own and so no tag to read.
/// `--proto =https` makes HTTPS the only protocol curl will speak: the URL
/// already says so, but the answer decides what a user is told to install, and
/// the flag keeps that true even if the URL or a config file ever says
/// otherwise.
fn fetch_latest_release() -> Result<LatestRelease> {
    let program = curl_program();
    let output = Command::new(&program)
        .args([
            "--silent",
            "--show-error",
            "--fail",
            "--no-location",
            "--proto",
            "=https",
            "--max-time",
            TIMEOUT_SECS,
            "--user-agent",
            USER_AGENT,
            "--output",
            NULL_DEVICE,
            "--write-out",
            "%{redirect_url}",
            LATEST_RELEASE_PAGE,
        ])
        .output()
        .with_context(|| {
            format!(
                "could not run {program}, which check-update needs to reach GitHub; \
                 install curl, or see the newest release at {RELEASES_PAGE}"
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        let detail = if detail.is_empty() {
            format!("curl exited with {}", output.status)
        } else {
            detail.to_string()
        };
        bail!(
            "could not reach GitHub to find the newest release ({detail}); \
             see it at {RELEASES_PAGE}"
        );
    }

    LatestRelease::from_redirect(&String::from_utf8_lossy(&output.stdout))
}

/// Compare the running build against the newest release.
fn check() -> Result<UpdateStatus> {
    let release = fetch_latest_release()?;
    // Tags carry a `v` prefix; Cargo versions do not.
    let latest = release.tag.strip_prefix('v').unwrap_or(&release.tag);
    let current = env!("CARGO_PKG_VERSION");
    Ok(UpdateStatus {
        update_available: is_newer(latest, current),
        current: current.to_string(),
        latest: latest.to_string(),
        release_url: release.url,
    })
}

/// Run `check-update`: report the comparison, and say how to act on it.
pub fn run_check_update(out: &OutputOpts) -> Result<()> {
    let status = check()?;
    output::emit_update_status(&status, out)?;
    if status.update_available {
        eprintln!("{}", upgrade_hint());
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

    /// What GitHub actually sends: the value of `%{redirect_url}` for
    /// `releases/latest`, captured from a real request when v0.5.2 was current.
    #[test]
    fn a_redirect_to_a_release_tag_names_the_release() {
        let url = "https://github.com/tiredithumans/crt-query/releases/tag/v0.5.2";
        let release = LatestRelease::from_redirect(url).expect("a release tag");
        assert_eq!(
            release,
            LatestRelease {
                tag: "v0.5.2".to_string(),
                url: url.to_string(),
            }
        );
        // curl prints no newline after `--write-out`, but nothing is lost by
        // tolerating one.
        let padded = LatestRelease::from_redirect(&format!("{url}\n")).unwrap();
        assert_eq!(
            padded.url, url,
            "surrounding whitespace is not part of the URL"
        );
    }

    /// The URL fetched and the prefix its answer is checked against have to
    /// name the same repository, or every check fails. They share one macro;
    /// this is what says so.
    #[test]
    fn the_fetched_url_and_the_accepted_prefix_name_the_same_repository() {
        let repo = LATEST_RELEASE_PAGE
            .strip_suffix("/latest")
            .expect("releases/latest");
        assert_eq!(repo, RELEASES_PAGE);
        assert_eq!(RELEASE_TAG_PREFIX, format!("{RELEASES_PAGE}/tag/"));
        assert!(RELEASES_PAGE.starts_with("https://github.com/"));
    }

    /// A repository with no published release redirects somewhere else, such
    /// as its releases list, and a request that was never redirected has no
    /// `Location` at all. The last path segment of either would read as a
    /// version — `releases`, `latest` — and `is_newer` reports any unparseable
    /// version as an update.
    #[test]
    fn a_redirect_that_names_no_release_is_an_error_not_a_version() {
        for redirect in [
            "",
            "\n",
            "https://github.com/tiredithumans/crt-query/releases",
            "https://github.com/tiredithumans/crt-query/releases/",
            "https://github.com/tiredithumans/crt-query/releases/latest",
            "https://github.com/tiredithumans/crt-query/releases/tag/",
        ] {
            let err = LatestRelease::from_redirect(redirect)
                .expect_err(&format!("{redirect:?} names no release"))
                .to_string();
            assert!(
                err.contains(RELEASES_PAGE),
                "the error has to point at the releases page:\n{err}"
            );
        }
    }

    /// Only this repository's tags, on github.com, over HTTPS. Matching the
    /// shape `/releases/tag/<tag>` anywhere would take a version from whatever
    /// a middlebox or a rename redirected to.
    #[test]
    fn a_tag_from_anywhere_else_is_refused() {
        for redirect in [
            "https://github.com/someone/else/releases/tag/v9.9.9",
            "https://github.com/tiredithumans/crt-query-fork/releases/tag/v9.9.9",
            "https://github.com.example/tiredithumans/crt-query/releases/tag/v9.9.9",
            "https://example.com/tiredithumans/crt-query/releases/tag/v9.9.9",
            "http://github.com/tiredithumans/crt-query/releases/tag/v9.9.9",
            "https://api.github.com/repos/tiredithumans/crt-query/releases/tag/v9.9.9",
        ] {
            assert!(
                LatestRelease::from_redirect(redirect).is_err(),
                "{redirect:?} is not a release of this repository"
            );
        }
    }

    /// A second path segment, a query or a fragment would mean the prefix
    /// matched something other than a bare tag, and anything a terminal acts
    /// on must not reach the line a human reads.
    #[test]
    fn a_tag_has_to_look_like_a_tag() {
        for tag in [
            "v1.0.0/extra",
            "v1.0.0?x=1",
            "v1.0.0#top",
            "v1.0%2F0",
            "v1.0 0",
            "v1.0.0\u{1b}[2J",
        ] {
            let redirect = format!("{RELEASE_TAG_PREFIX}{tag}");
            assert!(
                LatestRelease::from_redirect(&redirect).is_err(),
                "{tag:?} is not a tag this build should report"
            );
        }
        for tag in ["v1.2.3", "v1.2.3-rc.1", "v1.2.3+build_7", "1.2.3"] {
            let redirect = format!("{RELEASE_TAG_PREFIX}{tag}");
            let release = LatestRelease::from_redirect(&redirect)
                .unwrap_or_else(|e| panic!("{tag:?} is a plausible tag: {e}"));
            assert_eq!(release.tag, tag);
        }
    }

    /// The rejected URL is echoed so the error says what GitHub did, but it
    /// came off the network: its control characters are escaped, not printed.
    #[test]
    fn a_rejected_redirect_is_echoed_escaped() {
        let err = LatestRelease::from_redirect("https://example.com/\u{1b}[2J")
            .unwrap_err()
            .to_string();
        assert!(!err.contains('\u{1b}'), "a raw escape reached the message");
        assert!(err.contains(r"\u{1b}[2J"), "{err}");
    }

    /// The Windows curl choice, exercised on every host rather than only on the
    /// one CI leg that builds for Windows, so that a contributor on Linux or
    /// macOS sees it break too.
    fn windows_curl_with(root: Option<&str>, present: Option<&str>) -> String {
        windows_curl(root, |path| {
            Some(path.to_str().expect("a UTF-8 candidate")) == present
        })
    }

    /// Named by its full path, `System32`'s curl cannot be displaced by a
    /// `curl.exe` beside `crt-query.exe`, which a bare name would find first.
    #[test]
    fn windows_runs_system32_curl_by_its_full_path() {
        let system32 = Some(r"C:\Windows\System32\curl.exe");
        assert_eq!(
            windows_curl_with(Some(r"C:\Windows"), system32),
            r"C:\Windows\System32\curl.exe"
        );
        // A trailing separator does not produce a doubled one.
        assert_eq!(
            windows_curl_with(Some(r"C:\Windows\"), system32),
            r"C:\Windows\System32\curl.exe"
        );
        assert_eq!(
            windows_curl_with(Some("D:/WINDOWS"), Some(r"D:/WINDOWS\System32\curl.exe")),
            r"D:/WINDOWS\System32\curl.exe"
        );
    }

    /// Windows before 1803 has no `System32\curl.exe`. It still gets the bare
    /// name, so the subcommand keeps working there — with the old search.
    #[test]
    fn windows_falls_back_to_the_bare_name_without_system32_curl() {
        assert_eq!(windows_curl_with(Some(r"C:\Windows"), None), "curl");
        assert_eq!(windows_curl_with(None, None), "curl");
    }

    /// A `SystemRoot` that is not an absolute drive path is not trusted, and
    /// not even probed: relative forms resolve against the current directory,
    /// which is the planting problem by another route, and a UNC share would
    /// reach across the network to decide which program to start.
    #[test]
    fn windows_ignores_a_system_root_that_is_not_an_absolute_drive_path() {
        for root in [
            "",
            "Windows",
            r".\Windows",
            r"C:Windows",
            r"\Windows",
            r"\\server\share\Windows",
            // Absolute, but Windows never sets one, and reading the form
            // correctly would be parsing for a system that does not exist.
            r"\\?\C:\Windows",
            "C:",
        ] {
            let chosen = windows_curl(Some(root), |path| {
                panic!("{root:?} is not trustworthy, but {path:?} was probed")
            });
            assert_eq!(chosen, "curl", "{root:?}");
        }
    }

    #[test]
    fn the_upgrade_hint_names_a_verifying_path() {
        let hint = upgrade_hint();
        assert!(hint.contains("SHA256SUMS"));
        // `cargo install` re-resolves versions unless told not to, so without
        // this the from-source route builds a dependency set no gate has seen
        // — while every justfile recipe and release.yml pass --locked.
        assert!(
            hint.contains("cargo install --locked"),
            "the from-source hint must pin the lockfile:\n{hint}"
        );
    }

    /// A Homebrew install is upgraded with `brew upgrade`. Re-running
    /// `install.sh` instead drops a second, unmanaged copy in /usr/local/bin
    /// for Homebrew's own to shadow or be shadowed by — so the hint has to
    /// offer it where it runs, and must not on Windows, where it does not.
    #[test]
    fn homebrew_is_offered_exactly_where_homebrew_runs() {
        let hint = upgrade_hint();
        if cfg!(windows) {
            assert!(
                !hint.contains("brew"),
                "Homebrew does not run on Windows:\n{hint}"
            );
        } else {
            assert!(
                hint.contains("brew upgrade crt-query"),
                "a Homebrew install has no upgrade route in this hint:\n{hint}"
            );
        }
    }

    /// Every command starts in the same column, `From source:` included. A
    /// three-line block is only scannable if the labels are padded to match,
    /// and nothing else would notice if one stopped lining up.
    #[test]
    fn every_route_puts_its_command_in_the_same_column() {
        let hint = upgrade_hint();
        let columns: Vec<(usize, &str)> = hint
            .lines()
            .filter(|line| line.starts_with("  "))
            .map(|line| {
                let colon = line.find(':').expect("each route reads `Label: command`");
                let rest = &line[colon + 1..];
                (colon + 1 + (rest.len() - rest.trim_start().len()), line)
            })
            .collect();
        assert!(columns.len() >= 2, "expected several routes:\n{hint}");
        let (first, _) = columns[0];
        for (column, line) in &columns {
            assert_eq!(
                *column, first,
                "this route's command does not line up with the others:\n{line}"
            );
        }
    }

    /// The hint has to name a route that can install the binary printing it.
    /// A Windows build used to advertise `install.sh | sh` and nothing else,
    /// which is neither a command that host has nor a script that would install
    /// it. CI runs the suite on windows-latest, so both arms are exercised.
    #[test]
    fn the_upgrade_hint_names_a_route_for_the_platform_it_was_built_for() {
        let hint = upgrade_hint();
        let (wanted, wrong) = if cfg!(windows) {
            ("install.ps1", "install.sh")
        } else {
            ("install.sh", "install.ps1")
        };
        assert!(
            hint.contains(wanted),
            "this build's own install route ({wanted}) is missing:\n{hint}"
        );
        assert!(
            !hint.contains(wrong),
            "the hint offers {wrong}, which cannot install this binary:\n{hint}"
        );
    }
}
