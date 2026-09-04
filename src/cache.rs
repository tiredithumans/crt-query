//! A local, on-disk cache of query results.
//!
//! crt.sh is a free public service on donated infrastructure that refuses
//! connections and kills queries under load. The cheapest way to be a better
//! citizen of it — and the only one available without a second data source —
//! is to stop asking it the same question twice. A warm query is also the one
//! kind of query that survives an outage entirely, because it never dials.
//!
//! # What is stored
//!
//! One file per (statement, term, bind parameters) triple: the same granularity
//! [`crate::queries::fetch_by_term`] already loops at, so a multi-term run can
//! hit on some terms and miss on others.
//!
//! # Staleness
//!
//! `SEARCH_SQL` and `EXPIRING_SQL` evaluate their validity windows server-side
//! against `now()`, so a cached result set *is* the window as it stood when the
//! entry was written, not as it stands on replay. The drift is bounded by the
//! TTL and stays well below the day granularity of `--valid-since`, `--within`
//! and `--since-expired`, which is why the default TTL is short. `--refresh`
//! forces a re-fetch for callers who need the window recomputed now.
//!
//! # Failure policy
//!
//! A cache is an optimisation, so nothing in this module returns an error that
//! can fail a run. An unreadable entry, corrupt JSON, a stale format version or
//! an unwritable directory all degrade to a miss or a skipped write.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::queries::RawRow;

/// Bumped whenever the on-disk shape changes. An entry written by a different
/// version is a miss, not a parse error, so an upgrade degrades to a cold cache
/// rather than to a failing run.
const FORMAT_VERSION: u32 = 1;

/// Default lifetime for `search` and `expiring` results.
pub const DEFAULT_TTL: Duration = Duration::from_secs(60 * 60);

/// Marks an entry as holding a `cert` lookup, so that pruning can apply the
/// right lifetime without opening it.
const CERT_PREFIX: &str = "cert-";

/// Lifetime for a `cert <ID>` lookup. A certificate at a given crt.sh ID is
/// immutable — the record cannot change under us — so the short TTL that exists
/// to bound window drift buys nothing here.
pub const CERT_TTL: Duration = Duration::from_secs(60 * 60 * 24 * 30);

/// What a cached entry is keyed on. Held in full inside the entry and compared
/// exactly on read, so the filename hash only has to be a good spread: a
/// collision is a miss, never a wrong answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Key {
    /// `host:port/dbname`. Pointing `--host` elsewhere must not read entries
    /// written against crt.sh, or a private mirror and the public database
    /// would answer for each other.
    pub target: String,
    /// The statement text itself. Editing `SEARCH_SQL` or `EXPIRING_SQL`
    /// invalidates every entry that came from the old one, which extends the
    /// golden-SQL discipline in `queries/mod.rs` to cache correctness for free.
    pub sql: String,
    /// The search term, verbatim.
    pub term: String,
    /// The remaining bind parameters, rendered. `--valid-since 365` and
    /// `--valid-since 30` are different questions and must not share an entry.
    pub params: Vec<String>,
}

/// A cached payload, plus what is needed to age and validate it.
///
/// Generic because `search`/`expiring` cache `Vec<RawRow>` and `cert` caches a
/// single [`crate::queries::cert::CertDetail`]. The server clock is not a field
/// here: every `RawRow` already carries its own, so there is nothing to keep in
/// step, and a payload without one needs no clock at all.
#[derive(Debug, Serialize, Deserialize)]
struct Entry<T> {
    version: u32,
    key: Key,
    /// The client clock when this was written.
    fetched_at: DateTime<Utc>,
    payload: T,
}

/// How a run may use the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Read and write.
    Enabled,
    /// Skip the read, still write. `--refresh`.
    Refresh,
    /// Neither read nor write. `--no-cache`.
    Disabled,
}

impl Mode {
    fn reads(self) -> bool {
        matches!(self, Self::Enabled)
    }

    fn writes(self) -> bool {
        matches!(self, Self::Enabled | Self::Refresh)
    }
}

/// The cache, or a disabled stand-in when no directory could be determined.
pub struct Cache {
    dir: Option<PathBuf>,
    mode: Mode,
    ttl: Duration,
    /// Whether these entries are the long-lived `cert` kind.
    ///
    /// Tracked rather than inferred from `ttl`, which is configurable: pruning
    /// has to tell the two apart by name, and comparing durations would make
    /// `cache_ttl_secs = 2592001` silently reclassify every search result.
    long_lived: bool,
}

impl Cache {
    /// A cache rooted at the per-user cache directory.
    ///
    /// An environment that names no absolute cache directory yields a disabled
    /// cache rather than an error: the run still works, it just always misses.
    pub fn new(mode: Mode, ttl: Duration) -> Self {
        Self {
            dir: if mode == Mode::Disabled {
                None
            } else {
                cache_dir()
            },
            mode,
            ttl,
            long_lived: false,
        }
    }

    /// A cache rooted at an explicit directory.
    ///
    /// Test-only: the real constructor reads the environment, and a test that
    /// wrote entries into the caller's actual cache directory would be both
    /// destructive and order-dependent.
    #[cfg(test)]
    pub(crate) fn at(dir: PathBuf, mode: Mode, ttl: Duration) -> Self {
        Self {
            dir: Some(dir),
            mode,
            ttl,
            long_lived: false,
        }
    }

    /// The same cache, holding `cert` lookups under their own long lifetime.
    ///
    /// Those entries are named apart from the rest so that pruning can apply
    /// each lifetime to the entries it belongs to — see [`Cache::prune`].
    pub fn for_certs(&self) -> Self {
        Self {
            dir: self.dir.clone(),
            mode: self.mode,
            ttl: CERT_TTL,
            long_lived: true,
        }
    }

    /// How this cache may be used. Lets `main` assert the flag precedence
    /// without reaching into the field.
    #[cfg(test)]
    pub(crate) fn mode(&self) -> Mode {
        self.mode
    }

    /// How long an entry stays usable.
    #[cfg(test)]
    pub(crate) fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Where entries live, if anywhere.
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    /// Look `key` up, with how long ago it was written.
    ///
    /// `None` covers every failure as well as a genuine miss — see the module
    /// docs on why nothing here can fail a run.
    pub fn get<T: serde::de::DeserializeOwned>(&self, key: &Key) -> Option<(T, Duration)> {
        if !self.mode.reads() {
            return None;
        }
        let path = self.path(key)?;
        let text = std::fs::read_to_string(&path).ok()?;
        let entry: Entry<T> = serde_json::from_str(&text).ok()?;
        // The key is compared in full: the filename is only a hash, so this is
        // what makes a collision a miss rather than a wrong answer.
        if entry.version != FORMAT_VERSION || entry.key != *key {
            return None;
        }
        // A negative age means the clock moved backwards since the write.
        // `to_std` rejects it, which lands here as a miss — the right call,
        // since it would otherwise run a replayed clock backwards.
        let age = Utc::now()
            .signed_duration_since(entry.fetched_at)
            .to_std()
            .ok()?;
        if age > self.ttl {
            return None;
        }
        Some((entry.payload, age))
    }

    /// Store `payload` under `key`. Silently does nothing on any failure.
    pub fn put<T: Serialize>(&self, key: &Key, payload: &T) {
        if !self.mode.writes() {
            return;
        }
        let Some(path) = self.path(key) else { return };
        let Some(dir) = self.dir.as_deref() else {
            return;
        };
        let entry = Entry {
            version: FORMAT_VERSION,
            key: key.clone(),
            fetched_at: Utc::now(),
            payload,
        };
        let Ok(text) = serde_json::to_string(&entry) else {
            return;
        };
        if create_dir(dir).is_err() {
            return;
        }
        if write_atomic(&path, &text).is_ok() {
            self.prune();
        }
    }

    /// Look up cached rows, replaying their server clock forward.
    ///
    /// `server_now` rides on every row so that window membership and the
    /// EXPIRED/days-left labels are decided by a single clock — see the comment
    /// over `IDENTITY_QUERY`. Handing back an hour-old reading would reintroduce
    /// precisely the skew it exists to prevent, with `--skip-expired` free to
    /// print rows labelled EXPIRED. Advancing it by the entry's age keeps the
    /// client-to-server correction, which is the part a local `Utc::now()`
    /// cannot reproduce.
    pub fn get_rows(&self, key: &Key) -> Option<Vec<RawRow>> {
        let (mut rows, age) = self.get::<Vec<RawRow>>(key)?;
        let age = chrono::Duration::from_std(age).ok()?;
        for row in &mut rows {
            row.server_now += age;
        }
        Some(rows)
    }

    /// Delete every entry. Returns how many files went.
    pub fn clear(&self) -> std::io::Result<usize> {
        let Some(dir) = self.dir.as_deref() else {
            return Ok(0);
        };
        let mut removed = 0;
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            // Never created, so nothing to clear.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json") && std::fs::remove_file(&path).is_ok()
            {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Drop entries that have outlived their lifetime.
    ///
    /// Opportunistic, on write: bounded work on a directory we are already
    /// touching, and no background task to own. Files are judged by mtime
    /// rather than by parsing each one, so a corrupt entry ages out too.
    ///
    /// Each lifetime is applied only to the entries it governs, which is what
    /// the `cert-` prefix is for. Pruning everything under the short one would
    /// discard still-valid certificate records; pruning everything under the
    /// long one would leave a month of dead search results on disk.
    fn prune(&self) {
        let Some(dir) = self.dir.as_deref() else {
            return;
        };
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let now = SystemTime::now();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.extension().is_some_and(|e| e == "json") {
                continue;
            }
            let name = entry.file_name();
            let ttl = if name.to_string_lossy().starts_with(CERT_PREFIX) {
                CERT_TTL
            } else {
                self.ttl
            };
            let stale = entry
                .metadata()
                .and_then(|m| m.modified())
                .and_then(|m| now.duration_since(m).map_err(std::io::Error::other))
                .is_ok_and(|age| age > ttl);
            if stale {
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    fn path(&self, key: &Key) -> Option<PathBuf> {
        Some(self.dir.as_deref()?.join(self.filename(key)))
    }

    fn filename(&self, key: &Key) -> String {
        format!("{}{}.json", self.prefix(), digest(key))
    }

    /// What marks an entry as belonging to the long-lived `cert` lifetime.
    fn prefix(&self) -> &'static str {
        if self.long_lived { CERT_PREFIX } else { "" }
    }
}

/// FNV-1a over the key material, as 16 hex digits.
///
/// Hand-rolled rather than `std::hash::DefaultHasher`, whose output is
/// explicitly not guaranteed stable across Rust releases: a toolchain bump
/// would silently orphan every user's cache. This is not a cryptographic hash
/// and does not need to be — it names a file, and the full key inside the file
/// is what decides a hit.
fn digest(key: &Key) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    let mut eat = |bytes: &[u8]| {
        for b in bytes {
            hash ^= u64::from(*b);
            hash = hash.wrapping_mul(PRIME);
        }
    };
    // A length-prefixed field separator, so ("ab", "c") and ("a", "bc") differ.
    for field in [key.target.as_str(), key.sql.as_str(), key.term.as_str()] {
        eat(&(field.len() as u64).to_le_bytes());
        eat(field.as_bytes());
    }
    for param in &key.params {
        eat(&(param.len() as u64).to_le_bytes());
        eat(param.as_bytes());
    }
    format!("{hash:016x}")
}

/// Create the cache directory, owner-only where the platform has a notion of it.
///
/// The certificates are public; the list of domains this user searched for is
/// not. On a shared machine that list is the sensitive part of the cache.
fn create_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

/// Write through a scratch file and rename, so a reader never sees a partial
/// entry and an interrupted write leaves the previous one intact. Same approach
/// as the CSV destination in `output.rs`.
fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let scratch = path.with_extension("tmp");
    std::fs::write(&scratch, text)?;
    match std::fs::rename(&scratch, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&scratch);
            Err(e)
        }
    }
}

/// Where entries are kept, or `None` if the environment names no absolute
/// cache directory.
pub fn cache_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        Some(cache_path_in(&cache_root(std::env::var_os(
            "LOCALAPPDATA",
        ))?))
    }
    #[cfg(not(windows))]
    {
        Some(cache_path_in(&cache_root(
            std::env::var_os("XDG_CACHE_HOME"),
            std::env::var_os("HOME"),
        )?))
    }
}

/// The directory the cache is looked up under.
///
/// Absolute only, for the reason spelled out over `config::config_root`: a
/// relative `$XDG_CACHE_HOME` or `$HOME` resolves against the process's current
/// directory, so running inside a tree carrying `./.cache/crt-query` would read
/// and write entries the caller never put there. Answering a query from a cache
/// file that happened to be lying around in the working directory is a worse
/// failure than missing.
///
/// Taken as arguments rather than read here, so tests can reach the logic:
/// `std::env::set_var` is `unsafe` under edition 2024.
#[cfg(not(windows))]
fn cache_root(xdg: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    // `is_absolute` subsumes the emptiness check: "" is not an absolute path.
    xdg.map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            home.map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .map(|h| h.join(".cache"))
        })
}

/// Windows counterpart. `%LOCALAPPDATA%`, not the `%APPDATA%` the config file
/// uses: a cache is machine-local derived data and has no business roaming
/// between machines with a user's profile.
#[cfg(windows)]
fn cache_root(local_appdata: Option<OsString>) -> Option<PathBuf> {
    local_appdata.map(PathBuf::from).filter(|p| p.is_absolute())
}

fn cache_path_in(cache_root: &Path) -> PathBuf {
    // `$XDG_CACHE_HOME` is already a cache root, so the crate name is the whole
    // path. `%LOCALAPPDATA%` is not — it holds non-cache local state too — so
    // there the entries get their own subdirectory to sit in.
    #[cfg(windows)]
    {
        cache_root.join("crt-query").join("cache")
    }
    #[cfg(not(windows))]
    {
        cache_root.join("crt-query")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::utc;

    /// A fresh cache rooted in its own scratch directory, mirroring the
    /// `scratch_dir` pattern in `output.rs`: these write real files and the
    /// entry-counting assertions need to see only their own.
    fn scratch(name: &str, mode: Mode, ttl: Duration) -> Cache {
        let dir =
            std::env::temp_dir().join(format!("crt-query-cache-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Cache::at(dir, mode, ttl)
    }

    fn key(term: &str) -> Key {
        Key {
            target: "crt.sh:5432/certwatch".into(),
            sql: "SELECT 1".into(),
            term: term.into(),
            params: vec!["365".into()],
        }
    }

    fn row(id: i64, server_now: DateTime<Utc>) -> RawRow {
        RawRow {
            id,
            issuer_ca_id: Some(1),
            issuer_name: Some("Example CA".into()),
            matched_identity: "example.com".into(),
            common_name: Some("example.com".into()),
            serial: Some("00".into()),
            not_before: Some(utc(2026, 1, 1)),
            not_after: Some(utc(2026, 12, 31)),
            server_now,
        }
    }

    #[test]
    fn a_stored_result_comes_back() {
        let cache = scratch("roundtrip", Mode::Enabled, DEFAULT_TTL);
        let rows = vec![row(1, Utc::now()), row(2, Utc::now())];
        cache.put(&key("example.com"), &rows);
        let got = cache.get_rows(&key("example.com")).expect("a hit");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, 1);
        assert_eq!(got[1].matched_identity, "example.com");
    }

    /// An empty result is a real answer and worth keeping: a domain with no
    /// certificates should not re-ask a struggling database every time.
    #[test]
    fn an_empty_result_is_cached_rather_than_re_asked() {
        let cache = scratch("empty", Mode::Enabled, DEFAULT_TTL);
        cache.put(&key("nothing.invalid"), &Vec::<RawRow>::new());
        let got = cache.get_rows(&key("nothing.invalid"));
        assert!(got.is_some(), "an empty result must be a hit, not a miss");
        assert!(got.unwrap().is_empty());
    }

    /// The whole reason `server_now` rides on every row: window membership and
    /// the EXPIRED/days-left labels have to be decided by one clock, and it has
    /// to be the *server's*, corrected for skew. Replaying the stored reading
    /// verbatim would let `--skip-expired` print rows labelled EXPIRED, which
    /// is the bug the column exists to prevent.
    #[test]
    fn a_replayed_clock_advances_by_the_entrys_age() {
        let cache = scratch("clock", Mode::Enabled, DEFAULT_TTL);
        // A server an hour ahead of this client: the offset is the part a local
        // `Utc::now()` could never reproduce, so it must survive the round trip.
        let server = Utc::now() + chrono::Duration::hours(1);
        cache.put(&key("example.com"), &vec![row(1, server)]);

        let got = cache.get_rows(&key("example.com")).expect("a hit");
        let replayed = got[0].server_now;
        let offset = replayed - Utc::now();
        assert!(
            offset > chrono::Duration::minutes(59) && offset < chrono::Duration::minutes(61),
            "the server's one-hour lead should survive replay, got {offset}"
        );
        assert!(
            replayed >= server,
            "the replayed clock must never run backwards"
        );
    }

    #[test]
    fn an_entry_past_its_ttl_is_a_miss() {
        let cache = scratch("ttl", Mode::Enabled, Duration::from_secs(3600));
        cache.put(&key("example.com"), &vec![row(1, Utc::now())]);
        assert!(cache.get_rows(&key("example.com")).is_some());

        // Same directory, a TTL short enough that the entry just written is
        // already too old.
        let strict = Cache::at(cache.dir.clone().unwrap(), Mode::Enabled, Duration::ZERO);
        assert!(
            strict.get_rows(&key("example.com")).is_none(),
            "an entry older than the TTL must not be served"
        );
    }

    #[test]
    fn a_corrupt_entry_is_a_miss_and_not_an_error() {
        let cache = scratch("corrupt", Mode::Enabled, DEFAULT_TTL);
        let k = key("example.com");
        cache.put(&k, &vec![row(1, Utc::now())]);
        let path = cache.path(&k).unwrap();
        std::fs::write(&path, "{not json at all").unwrap();
        assert!(cache.get_rows(&k).is_none());

        // Truncation is the likelier corruption in practice.
        std::fs::write(&path, r#"{"version":1,"key":"#).unwrap();
        assert!(cache.get_rows(&k).is_none());
    }

    #[test]
    fn an_entry_from_another_format_version_is_a_miss() {
        let cache = scratch("version", Mode::Enabled, DEFAULT_TTL);
        let k = key("example.com");
        cache.put(&k, &vec![row(1, Utc::now())]);
        let path = cache.path(&k).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let bumped = text.replace(
            &format!("\"version\":{FORMAT_VERSION}"),
            &format!("\"version\":{}", FORMAT_VERSION + 1),
        );
        assert_ne!(text, bumped, "the version field should have been rewritten");
        std::fs::write(&path, bumped).unwrap();
        assert!(
            cache.get_rows(&k).is_none(),
            "an upgrade must degrade to a cold cache, not serve foreign data"
        );
    }

    /// The filename is only a hash, so the full key is compared on read. Force
    /// the collision by writing one key's entry to another key's path: the
    /// result must be a miss, never the wrong term's certificates.
    #[test]
    fn a_filename_collision_is_a_miss_and_never_a_wrong_answer() {
        let cache = scratch("collision", Mode::Enabled, DEFAULT_TTL);
        let mine = key("example.com");
        let theirs = key("example.net");
        cache.put(&theirs, &vec![row(99, Utc::now())]);

        let stolen = std::fs::read_to_string(cache.path(&theirs).unwrap()).unwrap();
        std::fs::write(cache.path(&mine).unwrap(), stolen).unwrap();

        assert!(
            cache.get_rows(&mine).is_none(),
            "an entry keyed to another term must never answer this one"
        );
    }

    #[test]
    fn every_part_of_the_key_changes_the_filename() {
        let base = key("example.com");
        let same = key("example.com");
        assert_eq!(digest(&base), digest(&same), "the digest must be stable");

        let mut host = base.clone();
        host.target = "localhost:5432/certwatch".into();
        let mut sql = base.clone();
        sql.sql = "SELECT 2".into();
        let mut term = base.clone();
        term.term = "example.net".into();
        let mut params = base.clone();
        params.params = vec!["30".into()];

        for (name, other) in [
            ("target", host),
            ("sql", sql),
            ("term", term),
            ("params", params),
        ] {
            assert_ne!(
                digest(&base),
                digest(&other),
                "changing {name} must change the digest"
            );
        }
    }

    /// Editing `SEARCH_SQL` or `EXPIRING_SQL` has to invalidate what the old
    /// statement produced, or a projection change would be served from entries
    /// that never carried the new columns. Keying on the statement text is what
    /// makes that automatic, so it gets a test of its own.
    #[test]
    fn changing_the_statement_invalidates_what_the_old_one_wrote() {
        let cache = scratch("sqlkey", Mode::Enabled, DEFAULT_TTL);
        let old = key("example.com");
        cache.put(&old, &vec![row(1, Utc::now())]);

        let mut edited = old.clone();
        edited.sql = format!("{} -- a new column", old.sql);
        assert!(cache.get_rows(&edited).is_none());
    }

    /// Length-prefixing each field. Without it the fields run together and
    /// ("ab", "c") hashes the same as ("a", "bc") — which for a (term, params)
    /// pair is a genuine reachable collision, not a theoretical one.
    #[test]
    fn the_digest_separates_adjacent_fields() {
        let mut a = key("ab");
        a.params = vec!["c".into()];
        let mut b = key("a");
        b.params = vec!["bc".into()];
        assert_ne!(digest(&a), digest(&b));
    }

    #[test]
    fn no_cache_neither_reads_nor_writes() {
        let seeded = scratch("disabled", Mode::Enabled, DEFAULT_TTL);
        seeded.put(&key("example.com"), &vec![row(1, Utc::now())]);

        let off = Cache::at(seeded.dir.clone().unwrap(), Mode::Disabled, DEFAULT_TTL);
        assert!(off.get_rows(&key("example.com")).is_none(), "must not read");

        off.put(&key("other.com"), &vec![row(2, Utc::now())]);
        assert!(
            seeded.get_rows(&key("other.com")).is_none(),
            "must not write"
        );
    }

    /// `--refresh` is not `--no-cache`: it exists to recompute a validity
    /// window, so it has to leave a fresh entry behind for the next run.
    #[test]
    fn refresh_skips_the_read_but_still_writes() {
        let cache = scratch("refresh", Mode::Enabled, DEFAULT_TTL);
        cache.put(&key("example.com"), &vec![row(1, Utc::now())]);

        let refreshing = Cache::at(cache.dir.clone().unwrap(), Mode::Refresh, DEFAULT_TTL);
        assert!(
            refreshing.get_rows(&key("example.com")).is_none(),
            "--refresh must ignore what is already there"
        );

        refreshing.put(&key("example.com"), &vec![row(42, Utc::now())]);
        let after = cache.get_rows(&key("example.com")).expect("rewritten");
        assert_eq!(after[0].id, 42, "--refresh must leave the fresh answer");
    }

    #[test]
    fn clear_removes_entries_and_tolerates_a_cache_that_was_never_written() {
        let cache = scratch("clear", Mode::Enabled, DEFAULT_TTL);
        cache.put(&key("a.example"), &vec![row(1, Utc::now())]);
        cache.put(&key("b.example"), &vec![row(2, Utc::now())]);
        assert_eq!(cache.clear().unwrap(), 2);
        assert!(cache.get_rows(&key("a.example")).is_none());
        assert_eq!(cache.clear().unwrap(), 0, "clearing twice is not an error");

        let missing = Cache::at(
            std::env::temp_dir().join("crt-query-cache-never-created"),
            Mode::Enabled,
            DEFAULT_TTL,
        );
        assert_eq!(missing.clear().unwrap(), 0);
    }

    /// A cache that cannot find a home must not fail the run.
    #[test]
    fn a_cache_with_nowhere_to_live_just_misses() {
        let nowhere = Cache {
            dir: None,
            mode: Mode::Enabled,
            ttl: DEFAULT_TTL,
            long_lived: false,
        };
        nowhere.put(&key("example.com"), &vec![row(1, Utc::now())]);
        assert!(nowhere.get_rows(&key("example.com")).is_none());
        assert_eq!(nowhere.clear().unwrap(), 0);
        assert!(nowhere.dir().is_none());
    }

    /// A scratch file left by an interrupted write is not an entry, and must
    /// not be mistaken for one or counted by `clear`.
    #[test]
    fn a_leftover_scratch_file_is_not_an_entry() {
        let cache = scratch("scratch", Mode::Enabled, DEFAULT_TTL);
        let k = key("example.com");
        cache.put(&k, &vec![row(1, Utc::now())]);
        let stray = cache.path(&k).unwrap().with_extension("tmp");
        std::fs::write(&stray, "half-written").unwrap();

        assert_eq!(cache.clear().unwrap(), 1, "only the .json entry counts");
        assert!(stray.exists(), "clear must not touch a foreign file");
        let _ = std::fs::remove_file(&stray);
    }

    #[cfg(not(windows))]
    mod root {
        use super::*;

        fn root(xdg: Option<&str>, home: Option<&str>) -> Option<String> {
            cache_root(xdg.map(Into::into), home.map(Into::into))
                .map(|p| p.to_string_lossy().into_owned())
        }

        #[test]
        fn xdg_wins_and_home_is_the_fallback() {
            assert_eq!(root(Some("/xdg"), Some("/home/u")).as_deref(), Some("/xdg"));
            assert_eq!(
                root(None, Some("/home/u")).as_deref(),
                Some("/home/u/.cache")
            );
            assert_eq!(root(None, None), None);
        }

        /// The same reasoning as `config::config_root`: a relative value
        /// resolves against the process's current directory, so a tree that
        /// happens to carry `./.cache/crt-query` would answer queries from
        /// entries the caller never wrote. Missing is the better failure.
        #[test]
        fn a_relative_or_empty_value_is_refused_not_resolved() {
            assert_eq!(root(Some("relative"), None), None);
            assert_eq!(root(Some(""), None), None);
            assert_eq!(root(Some(""), Some("")), None);
            assert_eq!(
                root(Some("rel"), Some("/home/u")).as_deref(),
                Some("/home/u/.cache")
            );
            assert_eq!(root(None, Some("home/u")), None);
        }
    }

    /// The two lifetimes have to prune independently: a `search` write must not
    /// carry off certificate records that are still good, and must not leave a
    /// month of its own dead entries behind either.
    #[test]
    fn each_lifetime_prunes_only_its_own_entries() {
        let searches = scratch("prune", Mode::Enabled, Duration::ZERO);
        let certs = searches.for_certs();

        let cert_key = key("12345");
        certs.put(&cert_key, &Some(1i64));
        assert!(
            certs.get::<Option<i64>>(&cert_key).is_some(),
            "a fresh cert entry should be readable"
        );

        // A search write with a zero lifetime: it prunes as it goes, so its own
        // entry is the one that should disappear.
        searches.put(&key("example.com"), &vec![row(1, Utc::now())]);
        searches.put(&key("other.example"), &vec![row(2, Utc::now())]);

        assert!(
            certs.get::<Option<i64>>(&cert_key).is_some(),
            "a search prune must not carry off certificate records"
        );
        let left: Vec<String> = std::fs::read_dir(searches.dir().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".json"))
            .collect();
        assert_eq!(
            left,
            vec![certs.filename(&cert_key)],
            "only the long-lived entry should survive a zero-lifetime search prune"
        );
    }

    /// A `cert` entry and a `search` entry that hash alike must still be two
    /// files, or one lifetime would silently overwrite the other.
    #[test]
    fn the_two_lifetimes_do_not_share_a_filename() {
        let searches = scratch("prefix", Mode::Enabled, DEFAULT_TTL);
        let certs = searches.for_certs();
        let k = key("example.com");
        assert_ne!(searches.filename(&k), certs.filename(&k));
        assert!(certs.filename(&k).starts_with(CERT_PREFIX));
    }
}
