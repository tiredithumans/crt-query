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

// ARRAY(SELECT ...) collapses the set-returning x509_altNames into a single
// text[] column, so this stays one row per certificate.
const CERT_SQL: &str = "\
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
 WHERE c.id = $1";

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
pub async fn run_cert(source: &mut Source, cache: &Cache, id: i64) -> Result<Option<CertDetail>> {
    let key = cert_key(source.cache_identity()?, id);
    if let Some(hit) = recall(cache, &key) {
        return Ok(hit);
    }
    let rows = source
        .db()
        .await?
        .query(&format!("crt.sh ID {id}"), CERT_SQL, &[(&id, Type::INT8)])
        .await?;
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

/// What a `cert <ID>` lookup is cached under. Both lifetimes use the same key;
/// the `cert-` filename prefix on the long-lived view is what keeps a found
/// certificate and a miss in separate files.
fn cert_key(identity: String, id: i64) -> Key {
    Key {
        target: identity,
        sql: CERT_SQL.to_string(),
        term: id.to_string(),
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
            cert_key(source.cache_identity().unwrap(), id)
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

            let got = run_cert(&mut source, &cache, 42)
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

            let got = run_cert(&mut source, &cache, 7)
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

            let Err(err) = run_cert(&mut source, &cache, 7).await else {
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
