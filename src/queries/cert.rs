use std::fmt;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio_postgres::types::Type;

use crate::cache::{Cache, Key};
use crate::db::Source;
use crate::output::{OutputRecord, csv_opt, csv_ts, expand_column, fmt_opt, fmt_ts};
use crate::queries::{column, timestamp};

/// Column index of the multi-valued SAN field within `cells()`.
const SANS_COL: usize = 9;

/// The projection and join both `cert` statements share; they differ only in
/// their `WHERE`. A macro rather than a `const` so the two statements can be
/// built with `concat!`, which takes literals only, and stay `&'static str`.
///
/// ARRAY(SELECT ...) collapses the set-returning x509_altNames into a single
/// text[] column, so this stays one row per certificate.
macro_rules! cert_select {
    () => {
        "\
SELECT c.id, c.issuer_ca_id, ca.name AS issuer_name,
       x509_subjectName(c.certificate) AS subject,
       x509_commonName(c.certificate) AS common_name,
       encode(x509_serialNumber(c.certificate), 'hex') AS serial,
       x509_notBefore(c.certificate) AS not_before,
       x509_notAfter(c.certificate) AS not_after,
       encode(digest(c.certificate, 'sha256'), 'hex') AS sha256_fingerprint,
       ARRAY(SELECT x509_altNames(c.certificate)) AS sans
  FROM certificate c
  LEFT JOIN ca ON ca.id = c.issuer_ca_id
"
    };
}

/// Look a certificate up by crt.sh ID. Byte-identical to the statement v0.5.x
/// sent, which matters beyond the golden file: the statement text is part of
/// every cache key, so changing it would orphan every cached certificate.
const CERT_SQL: &str = concat!(cert_select!(), " WHERE c.id = $1");

/// Look a certificate up by the SHA-256 of its DER encoding, bound as `bytea`.
///
/// The predicate is spelt exactly as crt.sh's expression index on the
/// certificate table is (`digest(certificate, 'sha256')`), which is what keeps
/// this an index lookup rather than a hash of every certificate ever logged;
/// the crt.sh website's own `?sha256=` search goes the same way. Comparing on
/// `encode(..., 'hex')` instead, or on the text form of the fingerprint, would
/// not match the index and would run into the guest database's statement
/// timeout.
const CERT_BY_SHA256_SQL: &str = concat!(
    cert_select!(),
    " WHERE digest(c.certificate, 'sha256') = $1"
);

/// What `cert` was asked to look up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CertRef {
    /// A crt.sh certificate ID.
    Id(i64),
    /// The SHA-256 fingerprint of the certificate: what browsers, `openssl
    /// x509 -fingerprint -sha256` and other CT tools show, and what a
    /// certificate is known by before anyone has looked up its crt.sh ID.
    Sha256([u8; 32]),
}

impl CertRef {
    /// Parse a `cert` argument: a crt.sh ID, or a SHA-256 fingerprint as 64
    /// hex digits, optionally colon-separated as `openssl` prints it.
    ///
    /// An ID is tried first and only up to 19 digits, the most an `i64` holds,
    /// so a fingerprint that happens to be all decimal digits is still read as
    /// a fingerprint. Anything else is a usage error, which clap reports with
    /// exit 2 — the reason exit 3 means "no such certificate" and nothing else.
    pub fn parse(arg: &str) -> Result<Self, String> {
        if !arg.is_empty() && arg.len() <= 19 && arg.bytes().all(|b| b.is_ascii_digit()) {
            return arg
                .parse()
                .map(Self::Id)
                .map_err(|e| format!("not a crt.sh ID: {e}"));
        }
        let hex: String = arg.chars().filter(|c| *c != ':').collect();
        if hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            let mut digest = [0_u8; 32];
            let (pairs, _) = hex.as_bytes().as_chunks::<2>();
            for (byte, pair) in digest.iter_mut().zip(pairs) {
                // Both are ASCII hex digits, checked above.
                let pair = std::str::from_utf8(pair).expect("ASCII");
                *byte = u8::from_str_radix(pair, 16).expect("hex digits");
            }
            return Ok(Self::Sha256(digest));
        }
        Err(
            "expected a crt.sh ID or a SHA-256 fingerprint (64 hex digits, colons allowed)"
                .to_string(),
        )
    }

    /// What goes in the cache key's `term`: the ID as v0.5.x wrote it, or the
    /// fingerprint in lowercase hex with a prefix no ID can have.
    fn term(&self) -> String {
        match self {
            Self::Id(id) => id.to_string(),
            Self::Sha256(digest) => format!("sha256:{}", hex(digest)),
        }
    }
}

/// How the lookup is named on stderr: "No certificate with crt.sh ID 42.",
/// "querying … for SHA-256 fingerprint 5c83…".
impl fmt::Display for CertRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Id(id) => write!(f, "crt.sh ID {id}"),
            Self::Sha256(digest) => write!(f, "SHA-256 fingerprint {}", hex(digest)),
        }
    }
}

/// Lowercase hex, the form crt.sh and this tool's `sha256_fingerprint` use.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

#[derive(Clone, Serialize, Deserialize)]
pub struct CertDetail {
    pub id: i64,
    pub issuer_ca_id: Option<i32>,
    pub issuer_name: Option<String>,
    pub subject: Option<String>,
    pub common_name: Option<String>,
    pub serial: Option<String>,
    pub not_before: Option<DateTime<Utc>>,
    pub not_after: Option<DateTime<Utc>>,
    pub sha256_fingerprint: Option<String>,
    pub sans: Vec<String>,
}

impl OutputRecord for CertDetail {
    fn headers() -> &'static [&'static str] {
        &[
            "crt.sh ID",
            "Issuer CA ID",
            "Issuer",
            "Subject",
            "Common Name",
            "Serial",
            "Not Before (UTC)",
            "Not After (UTC)",
            "SHA-256 Fingerprint",
            "SANs",
        ]
    }

    fn cells(&self) -> Vec<String> {
        vec![
            self.id.to_string(),
            fmt_opt(self.issuer_ca_id),
            fmt_opt(self.issuer_name.as_deref()),
            fmt_opt(self.subject.as_deref()),
            fmt_opt(self.common_name.as_deref()),
            fmt_opt(self.serial.as_deref()),
            fmt_ts(self.not_before.as_ref()),
            fmt_ts(self.not_after.as_ref()),
            fmt_opt(self.sha256_fingerprint.as_deref()),
            self.sans.join("; "),
        ]
    }

    fn csv_cells(&self) -> Vec<String> {
        vec![
            self.id.to_string(),
            csv_opt(self.issuer_ca_id),
            csv_opt(self.issuer_name.as_deref()),
            csv_opt(self.subject.as_deref()),
            csv_opt(self.common_name.as_deref()),
            csv_opt(self.serial.as_deref()),
            csv_ts(self.not_before.as_ref()),
            csv_ts(self.not_after.as_ref()),
            csv_opt(self.sha256_fingerprint.as_deref()),
            self.sans.join("; "),
        ]
    }

    fn csv_rows(&self) -> Vec<Vec<String>> {
        expand_column(self.csv_cells(), SANS_COL, &self.sans)
    }
}

/// Look one certificate up by its crt.sh ID.
///
/// A certificate that was found is cached under a much longer TTL than
/// `search` and `expiring`: the record at a given crt.sh ID is immutable, so
/// the short TTL that exists to bound validity-window drift buys nothing here.
///
/// A miss — no such ID — is cached too, so a typo'd ID in a loop is not
/// re-asked every time, but only under the short TTL. The guest database is a
/// replica that lags the crt.sh website, so an ID someone has just seen there
/// is a miss here for a while and then is not. v0.5.x cached the miss for the
/// full thirty days, and the lookup went on reporting "no such certificate"
/// (exit 3) for a month after the certificate arrived. See [`recall`] and
/// [`remember`] for which lifetime holds which answer.
pub async fn run_cert(
    source: &mut Source,
    cache: &Cache,
    lookup: &CertRef,
) -> Result<Option<CertDetail>> {
    let key = cert_key(source.cache_identity()?, lookup);
    if let Some(hit) = recall(cache, &key) {
        return Ok(hit);
    }
    let db = source.db().await?;
    let subject = lookup.to_string();
    let rows = match lookup {
        CertRef::Id(id) => db.query(&subject, CERT_SQL, &[(id, Type::INT8)]).await?,
        CertRef::Sha256(digest) => {
            let digest: &[u8] = digest;
            db.query(&subject, CERT_BY_SHA256_SQL, &[(&digest, Type::BYTEA)])
                .await?
        }
    };
    let detail = match rows.first() {
        None => None,
        Some(row) => Some(CertDetail {
            id: column(row, "id")?,
            issuer_ca_id: column(row, "issuer_ca_id")?,
            issuer_name: column(row, "issuer_name")?,
            subject: column(row, "subject")?,
            common_name: column(row, "common_name")?,
            serial: column(row, "serial")?,
            not_before: timestamp(row, "not_before")?,
            not_after: timestamp(row, "not_after")?,
            sha256_fingerprint: column(row, "sha256_fingerprint")?,
            sans: column(row, "sans")?,
        }),
    };
    remember(cache, &key, detail.as_ref());
    Ok(detail)
}

/// What a `cert` lookup is cached under. Both lifetimes use the same key; the
/// `cert-` filename prefix on the long-lived view is what keeps a found
/// certificate and a miss in separate files.
///
/// An ID lookup builds exactly the key v0.5.x did, so cached certificates
/// survive the upgrade. A fingerprint lookup has its own statement and term, so
/// the same certificate looked up both ways is two entries: correct, if
/// slightly wasteful, and it keeps a miss by one spelling from answering for
/// the other.
fn cert_key(identity: String, lookup: &CertRef) -> Key {
    let sql = match lookup {
        CertRef::Id(_) => CERT_SQL,
        CertRef::Sha256(_) => CERT_BY_SHA256_SQL,
    };
    Key {
        target: identity,
        sql: sql.to_string(),
        term: lookup.term(),
        params: Vec::new(),
    }
}

/// What the cache already knows about an ID: `Some(Some(_))` for a
/// certificate, `Some(None)` for a recent miss, and `None` when it has nothing
/// usable and the database has to be asked.
///
/// The long-lived view is read first, so a certificate that turns up after a
/// miss was cached (because a `--refresh` run found it) wins over the miss
/// still sitting in the short-lived view.
///
/// Each view accepts only the answer it exists to hold. The long-lived entry
/// is read as an `Option` purely so that a `null` there can be recognised and
/// passed over: v0.5.x wrote its misses into that view, and honouring them
/// would keep the month-long "no such certificate" alive for everyone who
/// upgraded. The short-lived entry is likewise only ever a miss; nothing
/// writes a certificate there, so one found there is not trusted either.
fn recall(cache: &Cache, key: &Key) -> Option<Option<CertDetail>> {
    if let Some((Some(detail), _)) = cache.for_certs().get::<Option<CertDetail>>(key) {
        return Some(Some(detail));
    }
    match cache.get::<Option<CertDetail>>(key) {
        Some((None, _)) => Some(None),
        _ => None,
    }
}

/// Store the answer under the lifetime it deserves: a found certificate in the
/// long-lived view, a miss in the ordinary short-lived one. See [`run_cert`]
/// on why a miss must not be kept for thirty days.
fn remember(cache: &Cache, key: &Key, detail: Option<&CertDetail>) {
    match detail {
        Some(detail) => cache.for_certs().put(key, &Some(detail)),
        None => cache.put(key, &None::<CertDetail>),
    }
}

/// The exact statement this module sends, for the golden-file test in
/// `queries::tests`. Reading it through one accessor keeps the snapshot tied
/// to what actually runs.
#[cfg(test)]
pub(crate) fn sql() -> &'static str {
    CERT_SQL
}

/// The fingerprint statement, for its golden-file test.
#[cfg(test)]
pub(crate) fn sha256_sql() -> &'static str {
    CERT_BY_SHA256_SQL
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detail(sans: &[&str]) -> CertDetail {
        CertDetail {
            id: 42,
            issuer_ca_id: Some(9),
            issuer_name: Some("Test CA".to_string()),
            subject: None,
            common_name: Some("example.com".to_string()),
            serial: Some("0a1b".to_string()),
            not_before: None,
            not_after: None,
            sha256_fingerprint: None,
            sans: sans.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn headers_and_cells_agree_in_arity() {
        assert_eq!(CertDetail::headers().len(), detail(&[]).cells().len());
    }

    mod caching {
        use super::*;
        use crate::cache::{DEFAULT_TTL, Mode};
        use crate::config::{Conn, DEFAULT_DBNAME};
        use std::path::PathBuf;
        use std::time::Duration;

        /// A source pointing somewhere nothing is listening, as in
        /// `queries::tests`: any dial fails fast and locally, so "did it
        /// dial?" is answerable offline.
        fn unreachable_source() -> Source {
            Source::new(Conn {
                host: "127.0.0.1".into(),
                port: 1,
                dbname: DEFAULT_DBNAME.into(),
                user: "guest".into(),
                db_url: None,
            })
        }

        /// A fresh directory of its own, since these write real entries.
        fn scratch(name: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!(
                "crt-query-cache-cert-{name}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        fn key_for(source: &Source, id: i64) -> Key {
            cert_key(source.cache_identity().unwrap(), &CertRef::Id(id))
        }

        /// The found certificate, served from the long-lived view through
        /// the function `main` actually calls. The closed port makes any
        /// dial a failure, so success is the proof that nothing dialled.
        #[tokio::test]
        async fn a_cached_certificate_is_served_without_a_connection() {
            let dir = scratch("found");
            let cache = Cache::at(dir.clone(), Mode::Enabled, DEFAULT_TTL);
            let mut source = unreachable_source();
            remember(&cache, &key_for(&source, 42), Some(&detail(&[])));

            let got = run_cert(&mut source, &cache, &CertRef::Id(42))
                .await
                .expect("a cached certificate must not need a connection");
            assert_eq!(got.map(|d| d.id), Some(42));
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// A recent miss is still an answer: a typo'd ID in a loop should not
        /// re-ask a struggling database every time.
        #[tokio::test]
        async fn a_cached_miss_is_served_without_a_connection() {
            let dir = scratch("miss");
            let cache = Cache::at(dir.clone(), Mode::Enabled, DEFAULT_TTL);
            let mut source = unreachable_source();
            remember(&cache, &key_for(&source, 7), None);

            let got = run_cert(&mut source, &cache, &CertRef::Id(7))
                .await
                .expect("a cached miss must not need a connection");
            assert!(got.is_none(), "a cached miss must stay a miss");
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// The reported bug. The guest database lags the crt.sh website, so a
        /// fresh ID is a miss for a while; a miss kept for thirty days went on
        /// answering exit 3 for a month after the certificate arrived.
        ///
        /// Proven through the lifetimes rather than the filenames: under a
        /// zero short lifetime the miss is gone and the certificate is not.
        #[test]
        fn a_miss_lives_under_the_short_lifetime_and_a_certificate_under_the_long() {
            let dir = scratch("lifetimes");
            let cache = Cache::at(dir.clone(), Mode::Enabled, DEFAULT_TTL);
            let source = unreachable_source();
            let (missing, found) = (key_for(&source, 7), key_for(&source, 42));
            remember(&cache, &missing, None);
            remember(&cache, &found, Some(&detail(&[])));

            assert!(
                cache
                    .for_certs()
                    .get::<Option<CertDetail>>(&missing)
                    .is_none(),
                "a miss was written into the long-lived view"
            );
            assert!(matches!(recall(&cache, &missing), Some(None)));

            let expired = Cache::at(dir.clone(), Mode::Enabled, Duration::ZERO);
            assert!(
                recall(&expired, &missing).is_none(),
                "a miss must age out under the short lifetime"
            );
            assert_eq!(
                recall(&expired, &found).flatten().map(|d| d.id),
                Some(42),
                "a found certificate keeps the long lifetime"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// v0.5.x wrote its misses into the long-lived view. Honouring one
        /// would keep the month-long "no such certificate" alive for everyone
        /// who upgraded, so the lookup has to pass over it and ask again.
        #[tokio::test]
        async fn a_long_lived_miss_left_by_v0_5_is_ignored() {
            let dir = scratch("legacy");
            let cache = Cache::at(dir.clone(), Mode::Enabled, DEFAULT_TTL);
            let mut source = unreachable_source();
            let key = key_for(&source, 7);
            cache.for_certs().put(&key, &None::<CertDetail>);
            assert!(recall(&cache, &key).is_none());

            let Err(err) = run_cert(&mut source, &cache, &CertRef::Id(7)).await else {
                panic!("a long-lived miss was served instead of re-asking");
            };
            assert!(
                format!("{err:#}").contains("could not connect"),
                "expected the lookup to reach for a connection, got: {err:#}"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// A certificate a `--refresh` run found after a miss was cached has to
        /// win over the miss, which is still sitting in the short-lived view
        /// until it ages out.
        #[test]
        fn a_certificate_in_the_short_lived_view_is_not_trusted() {
            // Nothing writes one there, so one found there came from somewhere
            // else, and serving it would give a certificate the short lifetime
            // meant for misses, or worse, one nobody fetched.
            let dir = scratch("short-found");
            let cache = Cache::at(dir.clone(), Mode::Enabled, DEFAULT_TTL);
            let key = key_for(&unreachable_source(), 42);
            cache.put(&key, &Some(detail(&[])));
            assert_eq!(recall(&cache, &key).map(|d| d.is_some()), None);
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn a_certificate_found_after_a_cached_miss_wins() {
            let dir = scratch("supersede");
            let cache = Cache::at(dir.clone(), Mode::Enabled, DEFAULT_TTL);
            let key = key_for(&unreachable_source(), 42);
            remember(&cache, &key, None);
            remember(&cache, &key, Some(&detail(&[])));

            assert!(
                cache.get::<Option<CertDetail>>(&key).is_some(),
                "the miss is still there, which is what makes this a test"
            );
            assert_eq!(recall(&cache, &key).flatten().map(|d| d.id), Some(42));
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    const FINGERPRINT: &str = "5c83f01af4edf38533f0da804bb740960120e9da1129216281a8542aea374bdd";

    #[test]
    fn a_cert_argument_is_an_id_or_a_fingerprint() {
        assert_eq!(CertRef::parse("22625564176"), Ok(CertRef::Id(22625564176)));
        let CertRef::Sha256(digest) = CertRef::parse(FINGERPRINT).unwrap() else {
            panic!("64 hex digits are a fingerprint");
        };
        assert_eq!(hex(&digest), FINGERPRINT);
        // openssl prints it upper-case and colon-separated.
        let openssl: String = FINGERPRINT
            .to_uppercase()
            .as_bytes()
            .chunks(2)
            .map(|p| std::str::from_utf8(p).unwrap())
            .collect::<Vec<_>>()
            .join(":");
        assert_eq!(CertRef::parse(&openssl), CertRef::parse(FINGERPRINT));
        // All decimal digits, but far too long for an ID: still a fingerprint.
        let digits = "1".repeat(64);
        assert!(matches!(CertRef::parse(&digits), Ok(CertRef::Sha256(_))));
    }

    /// A malformed argument is a usage error (exit 2 through clap), never a
    /// lookup that comes back "no such certificate" (exit 3).
    #[test]
    fn anything_else_is_refused() {
        for bad in [
            "",
            "notanumber",
            "-1",
            "12x",
            &FINGERPRINT[..63],
            &format!("{FINGERPRINT}0"),
            &FINGERPRINT.replacen('5', "g", 1),
            "99999999999999999999",
        ] {
            assert!(CertRef::parse(bad).is_err(), "accepted {bad:?}");
        }
    }

    /// An ID lookup has to build exactly the key v0.5.x built, or every cached
    /// certificate is orphaned by the upgrade; a fingerprint lookup must not
    /// share it.
    #[test]
    fn an_id_keeps_its_old_key_and_a_fingerprint_gets_its_own() {
        let id = cert_key("h:1/db".into(), &CertRef::Id(42));
        assert_eq!(id.sql, sql());
        assert_eq!(id.term, "42");
        let by_digest = cert_key("h:1/db".into(), &CertRef::parse(FINGERPRINT).unwrap());
        assert_eq!(by_digest.sql, sha256_sql());
        assert_eq!(by_digest.term, format!("sha256:{FINGERPRINT}"));
    }

    #[test]
    fn a_lookup_names_itself_the_way_the_messages_expect() {
        assert_eq!(CertRef::Id(42).to_string(), "crt.sh ID 42");
        assert_eq!(
            CertRef::parse(FINGERPRINT).unwrap().to_string(),
            format!("SHA-256 fingerprint {FINGERPRINT}")
        );
    }

    #[test]
    fn sans_col_points_at_the_sans_column() {
        assert_eq!(CertDetail::headers()[SANS_COL], "SANs");
    }

    #[test]
    fn csv_writes_one_row_per_san() {
        let rows = detail(&["example.com", "www.example.com"]).csv_rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][SANS_COL], "example.com");
        assert_eq!(rows[1][SANS_COL], "www.example.com");
    }

    #[test]
    fn a_certificate_without_sans_still_writes_one_row() {
        assert_eq!(detail(&[]).csv_rows().len(), 1);
    }

    /// The only Serialize record with no key-set assertion. ExpiringRow's is
    /// pinned in expiring.rs (transitively pinning SearchRow's eight through
    /// `#[serde(flatten)]`) and UpdateStatus's in output.rs. A plain field
    /// rename compiles — the columns are read by string literal, so the golden
    /// SQL is untouched — and silently renames a `cert --json` key.
    #[test]
    fn the_cert_json_document_keeps_its_ten_keys() {
        let value = serde_json::to_value(detail_fixture()).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "common_name",
                "id",
                "issuer_ca_id",
                "issuer_name",
                "not_after",
                "not_before",
                "sans",
                "serial",
                "sha256_fingerprint",
                "subject",
            ],
            "the `cert --json` shape changed"
        );
    }

    fn detail_fixture() -> CertDetail {
        CertDetail {
            id: 1,
            issuer_ca_id: Some(1),
            issuer_name: Some("Test CA".to_string()),
            subject: Some("CN=example.com".to_string()),
            common_name: Some("example.com".to_string()),
            serial: Some("0a".to_string()),
            not_before: None,
            not_after: None,
            sha256_fingerprint: Some("ff".to_string()),
            sans: vec!["example.com".to_string()],
        }
    }
}
