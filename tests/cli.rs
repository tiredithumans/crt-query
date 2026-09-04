//! End-to-end checks against the built binary.
//!
//! Everything above this file tests functions; nothing tested the program. That
//! gap is why an exit code could collide with clap's, and why `completions`
//! could write to a raw stdout for a release without anyone noticing.
//!
//! No dependency beyond the standard library: Cargo exports the binary's path
//! as `CARGO_BIN_EXE_<name>` for integration tests, which is all a process-level
//! assertion needs. Adding `assert_cmd` would buy nicer failure messages at the
//! cost of a dependency tree this crate deliberately does not have.
//!
//! **Every test here is offline.** crt.sh is a shared public service on donated
//! infrastructure; see CONTRIBUTING.md. The cases below either fail before any
//! connection is attempted or never need one, and the single case that reaches
//! the network layer points at `127.0.0.1` so nothing leaves the machine.

use std::path::PathBuf;
use std::process::{Command, Output};

/// clap's exit code for a usage error. Not ours to choose, which is the whole
/// reason `EXIT_NOT_FOUND` had to move off it.
const CLAP_USAGE_ERROR: i32 = 2;

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_crt-query"))
        .args(args)
        .output()
        .expect("failed to run the crt-query binary")
}

fn code(out: &Output) -> i32 {
    out.status.code().expect("process exited via a signal")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("crt-query-it-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir.join(name)
}

#[test]
fn help_and_version_succeed() {
    for args in [["--help"], ["--version"]] {
        let out = run(&args);
        assert_eq!(code(&out), 0, "{args:?} exited {}", code(&out));
        assert!(!stdout(&out).is_empty(), "{args:?} printed nothing");
    }
}

/// The collision that made a shell typo look like a missing certificate.
/// `cert` with a bad argument must report a *usage* error, and the code it
/// reports must not be one this tool also uses for a real result.
#[test]
fn a_malformed_cert_argument_is_a_usage_error_not_a_missing_certificate() {
    for bad in ["", "notanumber", "-1x"] {
        let out = run(&["cert", bad]);
        assert_eq!(
            code(&out),
            CLAP_USAGE_ERROR,
            "`cert {bad:?}` exited {} — a usage error must stay distinguishable \
             from \"no certificate with that ID\"",
            code(&out)
        );
    }
}

/// `crt-query search "$DOMAIN"` with `$DOMAIN` unset used to spend a
/// connection on the shared guest database and report "No certificates
/// found" — a result people act on — for a term that was never there. It is a
/// usage error, and the absence of "could not connect" in stderr is the
/// assertion that it is decided before the network is touched. (The sentinel
/// used to be "connection attempt", which the connect path no longer prints:
/// retries are silent now, so only the closing error names the host.)
#[test]
fn a_blank_term_is_a_usage_error_and_never_reaches_the_database() {
    for subcommand in ["search", "expiring"] {
        for blank in ["", "   "] {
            let out = run(&[
                subcommand,
                "example.com",
                blank,
                "--host",
                "127.0.0.1",
                "--port",
                "1",
            ]);
            let err = stderr(&out);
            assert_eq!(
                code(&out),
                CLAP_USAGE_ERROR,
                "`{subcommand} {blank:?}` exited {}:\n{err}",
                code(&out)
            );
            assert!(
                !err.contains("could not connect"),
                "a blank term was only rejected after a connection was attempted:\n{err}"
            );
        }
    }
}

#[test]
fn completions_are_written_through_the_normal_stdout_path() {
    let out = run(&["completions", "bash"]);
    assert_eq!(code(&out), 0);
    assert!(
        stdout(&out).contains("crt-query"),
        "the completion script does not name the binary"
    );
}

/// `completions` emits a shell script, not a record, so the output flags do not
/// apply to it — and it must not leave a file behind for a report it will never
/// write.
#[test]
fn completions_does_not_create_the_csv_destination() {
    let path = scratch("completions.csv");
    let _ = std::fs::remove_file(&path);
    let out = run(&["completions", "bash", "--csv", path.to_str().unwrap()]);
    assert_eq!(code(&out), 0);
    assert!(
        !path.exists(),
        "completions created {} for a report it never writes",
        path.display()
    );
}

/// The ordering that makes `--csv` safe to schedule: an unwritable destination
/// is caught before a connection is spent on the shared guest database. The
/// absence of "could not connect" in stderr is the actual assertion — the
/// error message alone would pass even if the check ran too late. That the
/// string appears when the connect path *does* run is pinned below, by
/// `a_spent_connect_reports_once_and_never_narrates_its_retries`; without it
/// this assertion would hold for a string the binary never prints.
#[test]
fn an_unwritable_csv_destination_fails_before_any_connection_is_attempted() {
    let out = run(&[
        "search",
        "example.com",
        "--csv",
        "/crt-query-no-such-directory/out.csv",
        "--host",
        "127.0.0.1",
        "--port",
        "1",
    ]);
    let err = stderr(&out);
    assert_eq!(code(&out), 1, "stderr was:\n{err}");
    assert!(
        err.contains("cannot write CSV to"),
        "expected the CSV precheck error, got:\n{err}"
    );
    assert!(
        !err.contains("could not connect"),
        "the CSV destination was checked only after a connection was attempted:\n{err}"
    );
}

/// The positive control for the two sentinels above, and the pin on the quiet
/// retries: a connect that never succeeds must say so exactly once, naming the
/// host, and must not have narrated the attempts it made on the way there. A
/// failed attempt used to print "connection attempt 1/3 to ... failed: db
/// error: ERROR: no more connections allowed (max_client_conn); retrying...",
/// which for the usual case — a later attempt connects — was an error message
/// for a run that then worked fine.
#[test]
fn a_spent_connect_reports_once_and_never_narrates_its_retries() {
    let out = run(&[
        "search",
        "example.com",
        "--host",
        "127.0.0.1",
        "--port",
        "1",
    ]);
    let err = stderr(&out);
    assert_eq!(code(&out), 1, "stderr was:\n{err}");
    assert!(
        err.contains("could not connect to 127.0.0.1:1"),
        "a spent connect did not name the host it could not reach:\n{err}"
    );
    for narration in ["retrying", "connection attempt", "attempt 1/"] {
        assert!(
            !err.contains(narration),
            "a failed attempt was announced as it happened ({narration:?}):\n{err}"
        );
    }
    // One report, not one per attempt.
    assert_eq!(
        err.matches("could not connect").count(),
        1,
        "the failure was reported more than once:\n{err}"
    );
}

/// A run that fails must not leave an empty file where the documented contract
/// promises a header row: a consumer testing for existence would find a report
/// and parse nothing out of it.
#[test]
fn a_failed_run_leaves_no_empty_report_behind() {
    let path = scratch("failed-run.csv");
    let _ = std::fs::remove_file(&path);
    let out = run(&[
        "search",
        "example.com",
        "--csv",
        path.to_str().unwrap(),
        "--host",
        "127.0.0.1",
        "--port",
        "1",
    ]);
    assert_eq!(code(&out), 1);
    assert!(
        !path.exists(),
        "left a {}-byte placeholder at {}",
        std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
        path.display()
    );
}

/// The previous run's report outlives a later failure. That is what
/// `truncate(false)` is for, and it is the half of the contract the fix above
/// must not have broken.
#[test]
fn an_existing_report_survives_a_later_failed_run() {
    let path = scratch("existing.csv");
    let original = "Matched Identities\nexample.com\n";
    std::fs::write(&path, original).expect("seed the previous report");
    let out = run(&[
        "search",
        "example.com",
        "--csv",
        path.to_str().unwrap(),
        "--host",
        "127.0.0.1",
        "--port",
        "1",
    ]);
    assert_eq!(code(&out), 1);
    assert_eq!(
        std::fs::read_to_string(&path).expect("report still present"),
        original,
        "a failed run destroyed the previous run's report"
    );
}

/// Not covered here, and deliberately: `EXIT_NOT_FOUND` (3) needs a real
/// certificate lookup, which needs crt.sh. `src/main.rs` pins that the constant
/// cannot collide with clap's 2; the cases above pin the other side of the
/// same contract from the outside.
#[test]
fn exit_code_documentation_is_present_for_the_case_this_suite_cannot_reach() {
    let readme = include_str!("../README.md");
    assert!(
        readme.contains("`3` no certificate with that crt.sh ID"),
        "README no longer documents exit 3; the only offline record of the \
         not-found contract is this line plus the constant in src/main.rs"
    );
}

/// `precheck_csv` creates the destination to prove it is writable, then removes
/// it again so a run that fails leaves no empty report. `exists()` follows a
/// symlink and `remove_file` does not, so for a dangling link it created the
/// *target* and deleted the *link* — losing a file the user made and leaving
/// behind exactly the empty report the check exists to prevent. The rotation
/// pattern below is the ordinary way to meet this.
#[test]
#[cfg(unix)]
fn a_dangling_report_symlink_survives_the_writability_check() {
    let link = scratch("latest.csv");
    let target = scratch("rotated-away.csv");
    let _ = std::fs::remove_file(&link);
    let _ = std::fs::remove_file(&target);
    std::os::unix::fs::symlink(&target, &link).expect("seed the rotation symlink");

    let out = run(&[
        "search",
        "example.com",
        "--csv",
        link.to_str().unwrap(),
        "--host",
        "127.0.0.1",
        "--port",
        "1",
    ]);
    assert_eq!(code(&out), 1, "stderr was:\n{}", stderr(&out));
    assert!(
        std::fs::symlink_metadata(&link).is_ok(),
        "the writability check deleted the user's symlink at {}",
        link.display()
    );
    assert!(
        !target.exists(),
        "left a {}-byte placeholder at {}, which is the empty report precheck_csv exists to prevent",
        std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0),
        target.display()
    );
}

/// `completions` is routed through `on_stdout` precisely so a closed reader
/// ends the run cleanly. `clap_complete::generate` panics on a write error, so
/// the pre-fix shape exits 101; asserting only "exit 0 and stdout mentions
/// crt-query" was true of that shape too, and so pinned nothing.
///
/// Dropping the child's stdout handle before waiting closes the pipe while the
/// ~15KB script is still being written. `| head -1` does not reproduce it: head
/// reads the whole script happily.
#[test]
fn a_closed_reader_ends_completions_cleanly_rather_than_panicking() {
    use std::process::Stdio;
    let mut child = Command::new(env!("CARGO_BIN_EXE_crt-query"))
        .args(["completions", "bash"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn crt-query");
    drop(child.stdout.take().expect("stdout was piped"));
    let status = child.wait().expect("wait for crt-query");
    // 0 because the output was complete as far as anyone was listening. The
    // point is what it is NOT: 101 from a panic, or a non-zero write error.
    assert_eq!(
        status.code(),
        Some(0),
        "a reader that went away should end the run cleanly, not kill it"
    );
}
