//! End-to-end cache checks against the built binary.
//!
//! The unit tests prove that `run_search` and `run_cert` never dial on a hit,
//! but they drive those functions with a cache built in the test. Nothing
//! proved that the shipped binary finds entries where it writes them: that the
//! directory it resolves from the environment, the key it builds from the
//! command line and the filename it hashes all agree with what an earlier run
//! left on disk. Any one of them drifting gives a cache that quietly never
//! hits, which looks exactly like a working one that is merely cold.
//!
//! **Every test here is offline.** Each one seeds a cache directory by hand
//! and runs the binary against `--host 127.0.0.1 --port 1`, where nothing
//! listens. A hit never dials; a miss fails fast with "could not connect", and
//! nothing ever reaches crt.sh.
//!
//! Each child is isolated from the developer's own machine. `XDG_CACHE_HOME`,
//! `XDG_CONFIG_HOME` and `HOME` (unix) and `LOCALAPPDATA` and `APPDATA`
//! (Windows) all point into a fresh directory per test, set on every platform
//! because a variable the other platform ignores costs nothing. Otherwise a
//! real cache entry could answer a case that is meant to miss, or a real
//! config file could send the run somewhere other than port 1.
//!
//! # This file pins the on-disk format, on purpose
//!
//! Entries are built here from first principles rather than through the crate,
//! which is bin-only and cannot be linked from here anyway: the key from the
//! golden SQL snapshots, the cache identity, the bind parameters rendered with
//! `Debug`, and the FNV-1a filename, all re-implemented. That duplicates
//! `src/cache.rs`, and the duplication is the point. Changing the digest, the
//! key or the entry shape orphans every user's cache without a word, and this
//! suite is what turns such a change from silent into a failing test, so that
//! it is made deliberately (with a `FORMAT_VERSION` bump where the shape
//! changed) rather than by accident.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};

/// The statements the binary sends, byte for byte. The golden-file tests in
/// `src/queries/mod.rs` hold these equal to the constants, and the constants
/// are part of every key, so these are the key material too.
const SEARCH_SQL: &str = include_str!("../src/queries/golden/search.sql");
const CERT_SQL: &str = include_str!("../src/queries/golden/cert.sql");
const EXPIRING_SQL: &str = include_str!("../src/queries/golden/expiring.sql");

/// `host:port/dbname` for `--host 127.0.0.1 --port 1` and the default
/// database, as `Source::cache_identity` renders it.
const IDENTITY: &str = "127.0.0.1:1/certwatch";

/// `FORMAT_VERSION` in `src/cache.rs`.
const FORMAT_VERSION: u32 = 1;

/// What marks a long-lived entry, which holds a found certificate.
const CERT_PREFIX: &str = "cert-";

/// The four fields of `cache::Key`, in the order the digest consumes them.
struct Key {
    target: String,
    sql: String,
    term: String,
    params: Vec<String>,
}

impl Key {
    /// The key `search <term>` builds with every flag at its default:
    /// `--valid-since 365` (an `i32`), no `--skip-expired`, `--limit 100` (an
    /// `i64`). Rendered with `Debug`, as `fetch_by_term` renders them.
    fn search(term: &str) -> Self {
        Self::search_with_limit(term, 100)
    }

    /// [`Key::search`] with `--limit <limit>`, which is part of the key.
    fn search_with_limit(term: &str, limit: i64) -> Self {
        Self {
            target: IDENTITY.to_string(),
            sql: SEARCH_SQL.to_string(),
            term: term.to_string(),
            params: vec![
                format!("{:?}", 365i32),
                format!("{:?}", false),
                format!("{:?}", limit),
            ],
        }
    }

    /// The key `expiring <domain>` builds with every flag at its default:
    /// `--within 30` and `--since-expired 30` (both `i32`), `--limit 500` (an
    /// `i64`).
    fn expiring(domain: &str) -> Self {
        Self {
            target: IDENTITY.to_string(),
            sql: EXPIRING_SQL.to_string(),
            term: domain.to_string(),
            params: vec![
                format!("{:?}", 30i32),
                format!("{:?}", 30i32),
                format!("{:?}", 500i64),
            ],
        }
    }

    /// The key `cert <id>` builds. Both lifetimes share it; only the filename
    /// prefix differs.
    fn cert(id: i64) -> Self {
        Self {
            target: IDENTITY.to_string(),
            sql: CERT_SQL.to_string(),
            term: id.to_string(),
            params: Vec::new(),
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "target": self.target,
            "sql": self.sql,
            "term": self.term,
            "params": self.params,
        })
    }

    /// FNV-1a over the length-prefixed fields, as 16 lowercase hex digits: a
    /// copy of `digest` in `src/cache.rs`, kept apart deliberately — see the
    /// module docs. If this and that ever disagree, every entry a user has is
    /// orphaned by the upgrade.
    fn digest(&self) -> String {
        const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const PRIME: u64 = 0x0000_0100_0000_01b3;
        let mut hash = OFFSET;
        let mut eat = |bytes: &[u8]| {
            for b in bytes {
                hash ^= u64::from(*b);
                hash = hash.wrapping_mul(PRIME);
            }
        };
        let fields = [&self.target, &self.sql, &self.term]
            .into_iter()
            .chain(&self.params);
        for field in fields {
            eat(&(field.len() as u64).to_le_bytes());
            eat(field.as_bytes());
        }
        format!("{hash:016x}")
    }
}

/// A fresh, private environment for one run of the binary, removed on drop.
struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    /// Unique per test and per test process: the suite runs in parallel.
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("crt-query-it-cache-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["xdg-cache", "xdg-config", "home", "localappdata", "appdata"] {
            std::fs::create_dir_all(root.join(dir)).expect("create sandbox directory");
        }
        Self { root }
    }

    /// Where the binary looks for entries under this sandbox's environment.
    fn cache_dir(&self) -> PathBuf {
        #[cfg(windows)]
        {
            self.root
                .join("localappdata")
                .join("crt-query")
                .join("cache")
        }
        #[cfg(not(windows))]
        {
            self.root.join("xdg-cache").join("crt-query")
        }
    }

    /// Write an entry exactly as `Cache::put` would, returning its path.
    fn seed(&self, prefix: &str, key: &Key, payload: Value) -> PathBuf {
        let entry = json!({
            "version": FORMAT_VERSION,
            "key": key.to_json(),
            "fetched_at": chrono::Utc::now(),
            "payload": payload,
        });
        let dir = self.cache_dir();
        std::fs::create_dir_all(&dir).expect("create the cache directory");
        let path = dir.join(format!("{prefix}{}.json", key.digest()));
        std::fs::write(&path, entry.to_string()).expect("seed the cache entry");
        path
    }

    /// The binary, with every directory it reads from the environment pointed
    /// into this sandbox.
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_crt-query"));
        command
            .env("XDG_CACHE_HOME", self.root.join("xdg-cache"))
            .env("XDG_CONFIG_HOME", self.root.join("xdg-config"))
            .env("HOME", self.root.join("home"))
            .env("LOCALAPPDATA", self.root.join("localappdata"))
            .env("APPDATA", self.root.join("appdata"));
        command
    }

    /// Run a query against a port nothing listens on, in this sandbox only.
    fn run(&self, args: &[&str]) -> Output {
        self.command()
            .args(args)
            .args(["--host", "127.0.0.1", "--port", "1"])
            .output()
            .expect("failed to run the crt-query binary")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
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

/// One identity row, as `RawRow` serialises it.
fn raw_row(id: i64, identity: &str) -> Value {
    json!({
        "id": id,
        "issuer_ca_id": 7,
        "issuer_name": "Example CA",
        "matched_identity": identity,
        "common_name": identity,
        "serial": "0a",
        "not_before": "2026-01-01T00:00:00Z",
        "not_after": "2026-12-31T00:00:00Z",
        "server_now": chrono::Utc::now(),
    })
}

/// A certificate, as `CertDetail` serialises it.
fn cert_detail(id: i64) -> Value {
    json!({
        "id": id,
        "issuer_ca_id": 7,
        "issuer_name": "Example CA",
        "subject": "CN=example.com",
        "common_name": "example.com",
        "serial": "0a",
        "not_before": "2026-01-01T00:00:00Z",
        "not_after": "2026-12-31T00:00:00Z",
        "sha256_fingerprint": "ff00ff",
        "sans": ["example.com", "www.example.com"],
    })
}

fn assert_never_dialled(out: &Output) {
    let err = stderr(out);
    assert!(
        !err.contains("could not connect"),
        "the run reached for a connection instead of using the cache:\n{err}"
    );
}

fn assert_dialled(out: &Output) {
    let err = stderr(out);
    assert_eq!(code(out), 1, "stderr was:\n{err}");
    assert!(
        err.contains("could not connect to 127.0.0.1:1"),
        "expected the run to reach for a connection, got:\n{err}"
    );
}

/// The whole feature, through the binary: a search whose answer is on disk is
/// served from it, and port 1 is never tried.
#[test]
fn a_cached_search_is_served_without_a_connection() {
    let sandbox = Sandbox::new("search");
    sandbox.seed(
        "",
        &Key::search("example.com"),
        json!([raw_row(4242, "example.com")]),
    );

    let out = sandbox.run(&["search", "example.com", "--json"]);
    assert_eq!(code(&out), 0, "stderr was:\n{}", stderr(&out));
    assert_never_dialled(&out);
    let rows: Value = serde_json::from_str(&stdout(&out)).expect("--json prints JSON");
    assert_eq!(rows.as_array().map(Vec::len), Some(1), "{rows}");
    assert_eq!(rows[0]["id"], 4242);
    assert_eq!(rows[0]["matched_identities"], json!(["example.com"]));
}

/// A found certificate lives in the long-lived view, under the `cert-` prefix.
#[test]
fn a_cached_certificate_is_served_without_a_connection() {
    let sandbox = Sandbox::new("cert-found");
    sandbox.seed(CERT_PREFIX, &Key::cert(42), cert_detail(42));

    let out = sandbox.run(&["cert", "42", "--json"]);
    assert_eq!(code(&out), 0, "stderr was:\n{}", stderr(&out));
    assert_never_dialled(&out);
    let detail: Value = serde_json::from_str(&stdout(&out)).expect("--json prints JSON");
    assert_eq!(detail["id"], 42);
    assert_eq!(detail["sha256_fingerprint"], "ff00ff");
}

/// A miss lives in the ordinary short-lived view, with no prefix, and is still
/// an answer: exit 3 and `null`, without a connection.
#[test]
fn a_cached_certificate_miss_exits_3_without_a_connection() {
    let sandbox = Sandbox::new("cert-miss");
    sandbox.seed("", &Key::cert(7), Value::Null);

    let out = sandbox.run(&["cert", "7", "--json"]);
    assert_eq!(code(&out), 3, "stderr was:\n{}", stderr(&out));
    assert_never_dialled(&out);
    assert_eq!(stdout(&out).trim(), "null");
    assert!(
        stderr(&out).contains("No certificate with crt.sh ID 7"),
        "{}",
        stderr(&out)
    );
}

/// v0.5.x wrote a miss into the long-lived view, where it answered exit 3 for
/// thirty days: long after a certificate the lagging replica had not yet seen
/// arrived. The binary has to pass over that entry and ask again.
///
/// Seeded under the current cache identity rather than the `host:port` key
/// v0.5.x used. That older key would be refused for its target alone, and the
/// test would pass without ever reaching the rule it exists for: that a
/// long-lived entry holding `null` is not an answer.
#[test]
fn a_long_lived_miss_left_by_v0_5_is_ignored() {
    let sandbox = Sandbox::new("cert-legacy-miss");
    sandbox.seed(CERT_PREFIX, &Key::cert(7), Value::Null);

    let out = sandbox.run(&["cert", "7", "--json"]);
    assert_dialled(&out);
    assert!(
        !stderr(&out).contains("No certificate with crt.sh ID"),
        "the legacy miss was served:\n{}",
        stderr(&out)
    );
    assert!(stdout(&out).trim().is_empty(), "{}", stdout(&out));
}

/// `--no-cache` must not read what is there. The entry below would answer the
/// search in full, so reaching for the connection is the proof it was skipped.
#[test]
fn no_cache_ignores_a_seeded_entry() {
    let sandbox = Sandbox::new("no-cache");
    let seeded = sandbox.seed(
        "",
        &Key::search("example.com"),
        json!([raw_row(4242, "example.com")]),
    );

    let out = sandbox.run(&["--no-cache", "search", "example.com", "--json"]);
    assert_dialled(&out);
    assert!(
        seeded.exists(),
        "--no-cache must not touch the entry either"
    );
}

/// The copy of the digest above is only a pin if it computes what the binary
/// computes. The cases that serve from the cache prove that end to end; this
/// fixes one value so a change on either side names itself. The same literal
/// is asserted against the real digest in `src/cache.rs`.
#[test]
fn the_filename_digest_is_pinned() {
    let key = Key {
        target: "crt.sh:5432/certwatch".to_string(),
        sql: "SELECT 1".to_string(),
        term: "example.com".to_string(),
        params: vec!["365".to_string()],
    };
    assert_eq!(key.digest(), PINNED_DIGEST);
}

/// FNV-1a of the key in `the_filename_digest_is_pinned`.
const PINNED_DIGEST: &str = "7366c5f8c0d1e192";

/// Guards the sandbox itself: an entry seeded where the binary does not look
/// would make every "never dialled" case above fail, but a sandbox that leaked
/// the developer's environment could make a "dialled" case pass for the wrong
/// reason. `cache path` names the directory the child actually resolved.
#[test]
fn the_sandbox_is_where_the_binary_looks() {
    let sandbox = Sandbox::new("where");
    let out = sandbox
        .command()
        .args(["cache", "path"])
        .output()
        .expect("failed to run the crt-query binary");
    assert_eq!(code(&out), 0, "stderr was:\n{}", stderr(&out));
    assert_eq!(Path::new(stdout(&out).trim()), sandbox.cache_dir());
}

/// `--quiet` is for cron, which mails whatever a job writes to stderr. A run
/// that filled its `--limit` window and wrote a CSV prints two informational
/// lines; with `--quiet` it prints none, and still writes the report.
///
/// The unquiet run is the control: without it this would pass for a build
/// that never printed those lines at all.
#[test]
fn quiet_silences_every_informational_line_but_keeps_the_output() {
    let sandbox = Sandbox::new("quiet");
    // Two identity rows of one certificate against --limit 2: the window is
    // full and collapses to one certificate, which is what prints the note.
    sandbox.seed(
        "",
        &Key::search_with_limit("example.com", 2),
        json!([raw_row(1, "example.com"), raw_row(1, "www.example.com")]),
    );
    let csv = sandbox.root.join("report.csv");
    let args = [
        "search",
        "example.com",
        "--limit",
        "2",
        "--csv",
        csv.to_str().unwrap(),
    ];

    let loud = sandbox.run(&args);
    assert_eq!(code(&loud), 0, "stderr was:\n{}", stderr(&loud));
    let err = stderr(&loud);
    assert!(err.contains("filled the server-side row window"), "{err}");
    assert!(err.contains("CSV row(s)"), "{err}");

    std::fs::remove_file(&csv).expect("the unquiet run wrote the report");
    let quiet = sandbox.run(&[&["--quiet"][..], &args[..]].concat());
    assert_eq!(code(&quiet), 0, "stderr was:\n{}", stderr(&quiet));
    assert_eq!(
        stderr(&quiet),
        "",
        "--quiet printed something that is not an error"
    );
    assert!(
        stdout(&quiet).contains("example.com"),
        "the table itself is output, not noise"
    );
    assert!(
        csv.exists(),
        "--quiet must not skip the report it was asked for"
    );
}

/// `expiring --fail-on-expiring` exits 4 when the report lists anything, so a
/// monitoring check can act on the status alone, and prints the report either
/// way. Without the flag the same report is an ordinary exit 0.
#[test]
fn fail_on_expiring_exits_4_only_when_there_is_something_to_report() {
    let sandbox = Sandbox::new("fail-on-expiring");
    sandbox.seed(
        "",
        &Key::expiring("busy.example"),
        json!([raw_row(7, "busy.example")]),
    );
    sandbox.seed("", &Key::expiring("quiet.example"), json!([]));

    let alert = sandbox.run(&["expiring", "busy.example", "--fail-on-expiring", "--json"]);
    assert_eq!(code(&alert), 4, "stderr was:\n{}", stderr(&alert));
    assert_never_dialled(&alert);
    let rows: Value = serde_json::from_str(&stdout(&alert)).expect("the report is still printed");
    assert_eq!(rows[0]["id"], 7);

    let plain = sandbox.run(&["expiring", "busy.example"]);
    assert_eq!(code(&plain), 0, "without the flag a report is a success");

    let empty = sandbox.run(&["expiring", "quiet.example", "--fail-on-expiring"]);
    assert_eq!(code(&empty), 0, "nothing to report is not an alert");
}

/// The alert survives a reader that stops early. The broken-pipe path used to
/// exit 0 from inside the writer, which for this flag would turn "certificates
/// are expiring" into "all clear" the moment someone piped it into `head`.
#[test]
fn fail_on_expiring_keeps_exit_4_when_the_reader_goes_away() {
    use std::process::Stdio;
    let sandbox = Sandbox::new("fail-on-expiring-pipe");
    // Enough rows that the JSON outgrows the pipe buffer, so the write is
    // still in progress when the reader goes away.
    let rows: Vec<Value> = (0..2000)
        .map(|i| {
            let mut row = raw_row(i, &format!("host{i}.busy.example"));
            row["serial"] = json!(format!("{i:08x}"));
            row
        })
        .collect();
    sandbox.seed("", &Key::expiring("busy.example"), Value::Array(rows));

    let mut child = sandbox
        .command()
        .args(["expiring", "busy.example", "--fail-on-expiring", "--json"])
        .args(["--host", "127.0.0.1", "--port", "1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn crt-query");
    drop(child.stdout.take().expect("stdout was piped"));
    let status = child.wait().expect("wait for crt-query");
    assert_eq!(
        status.code(),
        Some(4),
        "the alert was lost to a closed pipe"
    );
}
