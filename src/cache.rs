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
//! # Lifetimes
//!
//! Two, told apart by filename so that pruning never has to open an entry:
//!
//! - **Short** (an hour by default, `cache_ttl_secs` in the config file):
//!   `search` and `expiring` results, and a `cert` ID that was not found.
//! - **Long** ([`CERT_TTL`], thirty days, `cert-` prefix): a certificate that
//!   was found. The record at a crt.sh ID cannot change, so there is nothing
//!   for a short lifetime to protect.
//!
//! A miss is short-lived because it is not the same kind of fact. The guest
//! database is a replica that runs behind the crt.sh website, so an ID logged
//! minutes ago is a miss there for a while and then stops being one. v0.5.x
//! kept the miss for the full thirty days, and a user who looked a fresh ID up
//! too early was told "no such certificate", with exit 3, for a month after it
//! arrived.
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

/// Default short lifetime: `search` and `expiring` results, and a `cert` ID
/// that was not found.
pub const DEFAULT_TTL: Duration = Duration::from_secs(60 * 60);

/// Marks an entry as holding a found certificate, so that pruning can apply
/// the long lifetime without opening it.
///
/// A `cert` miss carries no prefix: it lives and is pruned under the short
/// lifetime like any other entry. See the module docs on why.
const CERT_PREFIX: &str = "cert-";

/// Lifetime for a certificate that `cert <ID>` found. A certificate at a given
/// crt.sh ID is immutable — the record cannot change under us — so the short
/// TTL that exists to bound window drift buys nothing here.
///
/// Only for a certificate that was found. An ID that was not found can start
/// existing once the lagging replica catches up, which is why a miss gets the
/// short lifetime instead.
pub const CERT_TTL: Duration = Duration::from_secs(60 * 60 * 24 * 30);

/// What a cached entry is keyed on. Held in full inside the entry and compared
/// exactly on read, so the filename hash only has to be a good spread: a
/// collision is a miss, never a wrong answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Key {
    /// `host:port/dbname`, from [`crate::db::Source::cache_identity`].
    /// Pointing `--host` elsewhere must not read entries written against
    /// crt.sh, or a private mirror and the public database would answer for
    /// each other, and the same goes for a second database behind one server.
    ///
    /// This field said `host:port/dbname` for a release while every caller
    /// filled it with the user-facing `host:port`, so the database half of
    /// that promise was never kept. The identity is a separate accessor now,
    /// and the user-facing target is not something a key can be built from.
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
    /// The short lifetime, even on the long-lived view.
    ///
    /// [`Cache::for_certs`] used to overwrite this with [`CERT_TTL`], so a
    /// prune that ran after writing a certificate judged every unprefixed
    /// entry by thirty days and left dead search results and `cert` misses in
    /// place. Keeping the short lifetime here and deriving the long one from
    /// `long_lived` lets every prune apply both correctly — see
    /// [`Cache::lifetime`].
    ttl: Duration,
    /// Whether these entries are the long-lived kind: found certificates.
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

    /// The same cache, holding found certificates under their own long
    /// lifetime.
    ///
    /// Those entries are named apart from the rest so that pruning can apply
    /// each lifetime to the entries it belongs to — see [`Cache::prune`]. A
    /// `cert` miss does not belong here: it goes through the ordinary cache,
    /// under the short lifetime. See the module docs.
    pub fn for_certs(&self) -> Self {
        Self {
            dir: self.dir.clone(),
            mode: self.mode,
            ttl: self.ttl,
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
        self.lifetime()
    }

    /// How long an entry written through this view stays usable.
    fn lifetime(&self) -> Duration {
        if self.long_lived { CERT_TTL } else { self.ttl }
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
        if age > self.lifetime() {
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
    /// long one would leave a month of dead search results on disk. Everything
    /// unprefixed, `cert` misses included, is judged by the short lifetime
    /// whichever view is doing the pruning.
    ///
    /// Scratch files go too, once they are older than the short lifetime and
    /// [`SCRATCH_GRACE`]. A write that was interrupted between the scratch
    /// file and the rename (a killed cron job, a full disk) leaves one behind,
    /// and nothing else would ever remove it: `clear` counts entries, and a
    /// scratch file is not one. Only names this module writes are touched —
    /// see [`is_scratch`] — so anything else in the directory is left alone.
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
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let ttl = if is_scratch(&name) {
                self.ttl.max(SCRATCH_GRACE)
            } else if !path.extension().is_some_and(|e| e == "json") {
                continue;
            } else if name.starts_with(CERT_PREFIX) {
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

    /// What marks an entry as belonging to the long lifetime: a found
    /// certificate.
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
///
/// `tests/cache.rs` carries its own copy of this function and seeds entries
/// with it for the real binary to find. The copy is deliberate: a change here
/// orphans every user's cache just as a toolchain bump would have, so it has
/// to fail a test and be made on purpose, not slip through as a refactor.
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
    let scratch = scratch_path(path);
    std::fs::write(&scratch, text)?;
    match std::fs::rename(&scratch, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&scratch);
            Err(e)
        }
    }
}

/// Where the entry at `path` is written before it is renamed into place:
/// `[cert-]<digest>.<pid>-<nanos>.tmp`, beside it; see [`scratch_tag`].
///
/// The process ID is there for the reason `output.rs`'s `scratch_beside` gives
/// for CSV. The scratch file used to be `[cert-]<digest>.tmp`, shared by every
/// process writing the same entry, and the README advertises `expiring --csv`
/// on a schedule, where two runs finishing the same query together is
/// ordinary. One run's rename could then move the other's half-written file
/// into place, leaving a truncated entry for every later run to read as
/// corrupt, and the other's rename failed on a file that had gone. With the
/// process ID each run renames only what it wrote, and whichever renames last
/// wins, which is no worse than one run replacing another's entry a moment
/// later. One process never races itself: everything runs in sequence on a
/// current-thread runtime.
fn scratch_path(path: &Path) -> PathBuf {
    path.with_extension(format!("{}.tmp", scratch_tag()))
}

/// `<pid>-<nanoseconds>`: what makes one writer's scratch file its own.
///
/// The process ID alone is not enough everywhere. Containers sharing a cache
/// volume each run their entrypoint as PID 1, so two of them finishing the same
/// query together would share `<digest>.1.tmp` and bring back the truncation
/// the process ID was added to prevent. The wall clock's nanoseconds separate
/// them; the process ID still separates two processes on one host that read
/// the same clock tick.
pub(crate) fn scratch_tag() -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    format!("{}-{nanos}", std::process::id())
}

/// The youngest a scratch file can be and still be pruned, whatever the short
/// lifetime says.
///
/// Scratch files are judged by the short lifetime, but `cache_ttl_secs` goes
/// down to zero, and a zero lifetime would let one run's prune delete another
/// run's scratch file between its write and its rename. No write this module
/// makes takes more than a moment, so a scratch file this old was abandoned.
const SCRATCH_GRACE: Duration = Duration::from_secs(10 * 60);

/// Whether `name` is a scratch file this module wrote: `[cert-]<digest>.tmp`
/// as v0.5.x named them, `[cert-]<digest>.<pid>.tmp` as the next version did,
/// or `[cert-]<digest>.<pid>-<nanos>.tmp` as [`scratch_path`] names them now.
///
/// Matched exactly rather than by the `.tmp` extension, because pruning
/// deletes what matches and the cache directory is not the only thing that
/// might hold a file ending in `.tmp`.
fn is_scratch(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".tmp") else {
        return false;
    };
    let stem = stem.strip_prefix(CERT_PREFIX).unwrap_or(stem);
    let (hash, pid) = match stem.split_once('.') {
        Some((hash, pid)) => (hash, Some(pid)),
        None => (stem, None),
    };
    let is_digest =
        hash.len() == 16 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    let is_tag = |tag: &str| {
        let (pid, nanos) = tag.split_once('-').unwrap_or((tag, "0"));
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        digits(pid) && digits(nanos)
    };
    let is_pid = pid.is_none_or(is_tag);
    is_digest && is_pid
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
        let strict = Cache::at(cache.dir.unwrap(), Mode::Enabled, Duration::ZERO);
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

    /// Every entry on every user's disk is named by this function, so its
    /// output is a format, not an implementation detail: a different digest
    /// for the same key is a cold cache for everyone who upgrades. The same
    /// literal is asserted against the copy in `tests/cache.rs`, which seeds
    /// entries for the real binary, so the two cannot drift apart unnoticed.
    #[test]
    fn the_digest_is_pinned() {
        assert_eq!(
            digest(&key("example.com")),
            "7366c5f8c0d1e192",
            "the filename digest changed; every existing cache entry is now \
             orphaned. If that is intended, update tests/cache.rs to match"
        );
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

    /// Wind a file's mtime back, which is all pruning looks at.
    fn backdate(path: &Path, by: Duration) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(SystemTime::now() - by)
            .unwrap();
    }

    /// A scratch file named for its entry alone was shared by every process
    /// writing that entry, so two scheduled runs finishing the same query
    /// together could rename each other's half-written file into place.
    #[test]
    fn the_scratch_file_is_named_for_the_process_writing_it() {
        let cache = scratch("scratch-pid", Mode::Enabled, DEFAULT_TTL);
        let k = key("example.com");
        let entry = cache.path(&k).unwrap();
        let scratch = scratch_path(&entry);
        let stem = entry.file_stem().unwrap().to_string_lossy().into_owned();
        let name = scratch.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with(&format!("{stem}.{}-", std::process::id()))
                && Path::new(&name).extension().is_some_and(|e| e == "tmp"),
            "the scratch name must carry this process's ID and a tick: {name}"
        );
        // Containers sharing a cache volume each run as PID 1, so the process
        // ID alone cannot keep two writers apart; the tick has to differ.
        std::thread::sleep(Duration::from_millis(1));
        assert_ne!(
            scratch_path(&entry),
            scratch,
            "two writes, one scratch name"
        );
        assert_eq!(
            scratch.parent(),
            entry.parent(),
            "rename needs one directory"
        );
        assert!(is_scratch(&scratch.file_name().unwrap().to_string_lossy()));

        // And a completed write leaves nothing of it behind.
        cache.put(&k, &vec![row(1, Utc::now())]);
        assert!(entry.exists());
        assert!(
            !scratch.exists(),
            "the scratch file should have been renamed"
        );
    }

    /// Only names this module writes are ever pruned as scratch files. The
    /// match is exact because a match gets deleted.
    #[test]
    fn only_the_scratch_names_this_module_writes_count_as_scratch() {
        let hash = "0123456789abcdef";
        for name in [
            format!("{hash}.tmp"),
            format!("{hash}.4242.tmp"),
            format!("{CERT_PREFIX}{hash}.tmp"),
            format!("{CERT_PREFIX}{hash}.4242.tmp"),
            format!("{hash}.4242-1758900000123456789.tmp"),
            format!("{CERT_PREFIX}{hash}.1-42.tmp"),
        ] {
            assert!(is_scratch(&name), "{name} is a scratch name");
        }
        for name in [
            "notes.tmp".to_string(),
            format!("{hash}.json"),
            format!("{CERT_PREFIX}{hash}.json"),
            format!("{hash}.pid.tmp"),
            format!("{hash}..tmp"),
            format!("{hash}.1.2.tmp"),
            "0123456789abcde.tmp".to_string(),
            "0123456789ABCDEF.tmp".to_string(),
            format!(".{hash}.1.tmp"),
            format!("{hash}.1-.tmp"),
            format!("{hash}.-1.tmp"),
            format!("{hash}.1-x.tmp"),
        ] {
            assert!(!is_scratch(&name), "{name} is not a scratch name");
        }
    }

    /// An interrupted write leaves its scratch file behind, and nothing else
    /// ever removes it. Once it is older than the short lifetime it is
    /// abandoned, whichever process or release left it.
    #[test]
    fn an_abandoned_scratch_file_is_pruned_once_older_than_the_short_lifetime() {
        let cache = scratch("scratch-prune", Mode::Enabled, DEFAULT_TTL);
        let dir = cache.dir.clone().unwrap();
        let hash = digest(&key("abandoned.example"));
        let abandoned = [
            dir.join(format!("{hash}.999999.tmp")),
            dir.join(format!("{hash}.tmp")),
            dir.join(format!("{CERT_PREFIX}{hash}.12.tmp")),
        ];
        let recent = dir.join(format!("{hash}.888888.tmp"));
        let foreign = dir.join("notes.tmp");
        for path in abandoned.iter().chain([&recent, &foreign]) {
            std::fs::write(path, "half-written").unwrap();
            backdate(path, DEFAULT_TTL + Duration::from_secs(3600));
        }
        // Younger than the short lifetime, so possibly still some run's.
        backdate(&recent, DEFAULT_TTL / 2);

        // Any write prunes.
        cache.put(&key("example.com"), &vec![row(1, Utc::now())]);

        for path in &abandoned {
            assert!(!path.exists(), "{} was left behind", path.display());
        }
        assert!(recent.exists(), "a scratch file inside the lifetime went");
        assert!(foreign.exists(), "a file this module never writes went");
    }

    /// The lifetime is configurable down to zero, and one run's prune must not
    /// delete another run's scratch file between its write and its rename.
    #[test]
    fn a_fresh_scratch_file_survives_a_prune_even_under_a_zero_lifetime() {
        let cache = scratch("scratch-fresh", Mode::Enabled, Duration::ZERO);
        let dir = cache.dir.clone().unwrap();
        let hash = digest(&key("in-flight.example"));
        let fresh = dir.join(format!("{hash}.999999.tmp"));
        let old = dir.join(format!("{hash}.888888.tmp"));
        std::fs::write(&fresh, "being written").unwrap();
        std::fs::write(&old, "abandoned").unwrap();
        backdate(&old, SCRATCH_GRACE + Duration::from_secs(60));

        cache.put(&key("example.com"), &vec![row(1, Utc::now())]);

        assert!(fresh.exists(), "a prune reached a write still in flight");
        assert!(
            !old.exists(),
            "the grace period is a floor, not a reason to keep everything"
        );
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
            .filter(|n| Path::new(n).extension().is_some_and(|e| e == "json"))
            .collect();
        assert_eq!(
            left,
            vec![certs.filename(&cert_key)],
            "only the long-lived entry should survive a zero-lifetime search prune"
        );
    }

    /// The other direction. `for_certs` used to replace the short lifetime
    /// with the long one, so a prune that ran after a certificate write judged
    /// every unprefixed entry by thirty days and kept dead search results and
    /// `cert` misses around. It has to apply the short lifetime to those
    /// whichever view is pruning.
    #[test]
    fn a_prune_after_a_certificate_write_still_applies_the_short_lifetime() {
        // Written under the default lifetime, so its own prune keeps it.
        let lenient = scratch("prune-certs", Mode::Enabled, DEFAULT_TTL);
        let stale = key("example.com");
        lenient.put(&stale, &vec![row(1, Utc::now())]);
        assert!(lenient.path(&stale).unwrap().exists());

        // The same directory under a zero short lifetime: writing a
        // certificate through the long-lived view prunes as it goes.
        let strict = Cache::at(lenient.dir.clone().unwrap(), Mode::Enabled, Duration::ZERO);
        let certs = strict.for_certs();
        let cert_key = key("12345");
        certs.put(&cert_key, &Some(1i64));

        assert!(
            !lenient.path(&stale).unwrap().exists(),
            "an unprefixed entry outlived the short lifetime because the prune \
             came from the long-lived view"
        );
        assert!(
            certs.get::<Option<i64>>(&cert_key).is_some(),
            "the long lifetime still governs the certificate itself"
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
