pub mod cert;
pub mod expiring;
pub mod search;

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio_postgres::Row;
use tokio_postgres::types::{FromSql, ToSql, Type};

use crate::cache::{Cache, Key};
use crate::db::Source;
use crate::queries::search::SearchRow;

/// Projection and index-driven `WHERE` clause shared by `search` and
/// `expiring`, which differ only in the validity predicates they append.
///
/// The tsquery predicate drives the full-text index; a bare ILIKE over the
/// table would hit the guest DB's statement timeout. No server-side ORDER
/// BY/DISTINCT for the same reason: LIMIT must be able to terminate early.
///
/// `ESCAPE ''` turns off Postgres' default backslash escape, leaving `%` and
/// `_` as the only metacharacters — which is what `--help` documents. Without
/// it every backslash in the term is swallowed and the character after it is
/// taken literally, so `a\b` searches for `ab` and a trailing backslash builds
/// a pattern that cannot match at all. Either way the run reports "No
/// certificates found", which is a result people act on. Identity terms — a DN
/// fragment, an email SAN — are where a backslash actually turns up.
///
/// `server_now` rides along on every row so that window membership and the
/// EXPIRED/days-left labels are decided by a single clock. Comparing a
/// server-side `now()` against a client-side one sampled after the query
/// returns lets `--skip-expired` print rows labelled EXPIRED.
pub const IDENTITY_QUERY: &str = "\
SELECT cai.certificate_id AS id, cai.issuer_ca_id, ca.name AS issuer_name,
       cai.name_value AS matched_identity,
       x509_commonName(cai.certificate) AS common_name,
       encode(x509_serialNumber(cai.certificate), 'hex') AS serial,
       x509_notBefore(cai.certificate) AS not_before,
       x509_notAfter(cai.certificate) AS not_after,
       (now() AT TIME ZONE 'UTC') AS server_now
  FROM certificate_and_identities cai
  LEFT JOIN ca ON ca.id = cai.issuer_ca_id
 WHERE plainto_tsquery('certwatch', $1) @@ identities(cai.certificate)
   AND cai.name_value ILIKE ('%' || $1 || '%') ESCAPE ''";

/// Read a column, naming it if the type or nullability does not match.
pub fn column<'a, T: FromSql<'a>>(row: &'a Row, name: &str) -> Result<T> {
    row.try_get(name)
        .with_context(|| format!("reading column `{name}`"))
}

/// Read a `timestamp` column. crt.sh stores certificate validity as UTC-naive
/// timestamps; attaching the zone here keeps every downstream value aware, so
/// JSON carries a `Z` instead of a bare local-looking string.
pub fn timestamp(row: &Row, name: &str) -> Result<Option<DateTime<Utc>>> {
    Ok(column::<Option<NaiveDateTime>>(row, name)?.map(|t| t.and_utc()))
}

/// One identity-match row as returned by the search/expiring SQL.
///
/// Serializable because [`crate::cache`] persists these verbatim. That makes
/// the field set an on-disk format: adding, removing or renaming one changes
/// what old entries mean, so it comes with a `FORMAT_VERSION` bump.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RawRow {
    pub id: i64,
    pub issuer_ca_id: Option<i32>,
    pub issuer_name: Option<String>,
    pub matched_identity: String,
    pub common_name: Option<String>,
    pub serial: Option<String>,
    pub not_before: Option<DateTime<Utc>>,
    pub not_after: Option<DateTime<Utc>>,
    /// The server's clock at query time, identical across every row.
    pub server_now: DateTime<Utc>,
}

impl RawRow {
    pub fn from_pg(row: &Row) -> Result<Self> {
        Ok(Self {
            id: column(row, "id")?,
            issuer_ca_id: column(row, "issuer_ca_id")?,
            issuer_name: column(row, "issuer_name")?,
            matched_identity: column(row, "matched_identity")?,
            common_name: column(row, "common_name")?,
            serial: column(row, "serial")?,
            not_before: timestamp(row, "not_before")?,
            not_after: timestamp(row, "not_after")?,
            server_now: timestamp(row, "server_now")?
                .context("server returned a NULL clock reading")?,
        })
    }
}

/// What a fetch returned, and whether the server-side row window filled up.
///
/// The flag is the difference between "crt.sh holds nothing more" and
/// "`--limit` stopped early", which the row count alone cannot tell apart once
/// [`to_rows`] has collapsed it.
#[derive(Debug)]
pub struct Fetched {
    pub rows: Vec<RawRow>,
    /// The terms whose window came back full.
    ///
    /// Names rather than a flag, because `--limit` is per term: one busy term
    /// fills its window while the rest come back short, and it is that term
    /// the limit has to be raised for. A bare `true` would leave the caller
    /// telling someone to widen a search without saying which half of it.
    pub saturated: Vec<String>,
}

/// A finished report, plus enough of what it cost to build for the caller to
/// tell a short answer from a truncated one.
pub struct Report<T> {
    pub rows: Vec<T>,
    /// Identity rows the window held, before [`to_rows`] collapsed them.
    pub raw_rows: usize,
    /// The terms that filled their `--limit` window.
    pub saturated: Vec<String>,
}

impl<T> Report<T> {
    /// Whether this result is short because the row window filled, rather than
    /// because crt.sh holds nothing more.
    ///
    /// `--limit` bounds identity rows, not certificates, and the collapse in
    /// [`to_rows`] runs only after the server has already spent the window. A
    /// full window that then collapses is indistinguishable, from the output
    /// alone, from a name that genuinely has a handful of certificates — which
    /// is the confusion this exists to name.
    ///
    /// Both halves are required. A full window that collapsed nothing handed
    /// the caller exactly the rows they asked for, and a window that never
    /// filled has nothing behind it to report.
    pub fn window_hid_certificates(&self) -> bool {
        !self.saturated.is_empty() && self.rows.len() < self.raw_rows
    }
}

/// Run an identity statement once per term and collect every row it returns.
///
/// One statement per term, in sequence, for the reasons CONTRIBUTING.md
/// spells out: the tsquery predicate has to stay index-driven to survive the
/// guest database's statement timeout, so the terms cannot be folded into one
/// `ANY` predicate; `--limit` is therefore per term, so a busy one cannot
/// crowd the rest out of the window; and this tool holds exactly one
/// connection to a database with a connection limit, so the terms queue
/// rather than fanning out.
///
/// `$1` is always the term. `extra` binds `$2` onwards and is the same for
/// every term — `search` and `expiring` differ only there.
///
/// `limit` is the row cap the caller has already bound inside `extra`, passed
/// again because `extra` is opaque here and a full window has to be recognised
/// to be reported.
pub async fn fetch_by_term(
    source: &mut Source,
    cache: &Cache,
    terms: &[String],
    sql: &str,
    extra: &[(&(dyn ToSql + Sync), Type)],
    limit: i64,
) -> Result<Fetched> {
    // The bind parameters past `$1` are identical for every term, so they are
    // rendered once and shared by every key built below.
    let params_key: Vec<String> = extra.iter().map(|(v, _)| format!("{v:?}")).collect();
    let target = source.target()?;

    let mut raw = Vec::new();
    let mut saturated = Vec::new();
    for term in terms {
        let key = Key {
            target: target.clone(),
            sql: sql.to_string(),
            term: term.clone(),
            params: params_key.clone(),
        };
        if let Some(hit) = cache.get_rows(&key) {
            // The limit is part of the key, so a hit was written under this
            // same cap and its length means what a fresh fetch's would.
            if hit.len() as i64 >= limit {
                saturated.push(term.clone());
            }
            raw.extend(hit);
            continue;
        }
        let mut params: Vec<(&(dyn ToSql + Sync), Type)> = Vec::with_capacity(extra.len() + 1);
        params.push((term, Type::TEXT));
        params.extend(extra.iter().map(|(value, ty)| (*value, ty.clone())));
        // The first miss is what dials: a run whose terms all hit never gets
        // here, and so never spends a client slot on the guest database.
        let rows = source
            .db()
            .await?
            .query(&format!("\"{term}\""), sql, &params)
            .await?;
        let mut fetched = Vec::with_capacity(rows.len());
        for row in &rows {
            fetched.push(RawRow::from_pg(row)?);
        }
        cache.put(&key, &fetched);
        if fetched.len() as i64 >= limit {
            saturated.push(term.clone());
        }
        raw.extend(fetched);
    }
    Ok(Fetched {
        rows: raw,
        saturated,
    })
}

/// Collapse raw identity rows into one row per certificate.
///
/// With `dedupe`:
/// 1. rows are grouped by crt.sh certificate ID, merging matched identities;
/// 2. precertificate/leaf pairs are collapsed on (issuer_ca_id, serial) —
///    RFC 6962 requires both to carry the same serial — keeping the lowest
///    crt.sh ID.
///
/// Step 2 only collapses a pair whose validity windows agree. RFC 6962 gives a
/// precertificate and its leaf the same notBefore/notAfter as well as the same
/// serial, so a pair that disagrees is a serial collision between genuinely
/// different certificates; merging those would drop one certificate entirely
/// and leave the survivor advertising identities it does not carry.
///
/// Without `dedupe`, each raw row becomes one output row. Dedup is best-effort:
/// rows beyond the server-side LIMIT are never seen.
pub fn to_rows(raw: Vec<RawRow>, dedupe: bool) -> Vec<SearchRow> {
    if !dedupe {
        return raw.into_iter().map(SearchRow::from).collect();
    }

    let mut by_id: BTreeMap<i64, SearchRow> = BTreeMap::new();
    for r in raw {
        match by_id.entry(r.id) {
            Entry::Vacant(v) => {
                v.insert(SearchRow::from(r));
            }
            Entry::Occupied(mut o) => o.get_mut().merge_identity(r.matched_identity),
        }
    }

    // Ascending ID order means the first row of each group is the lowest ID.
    //
    // Each serial holds a list, not a single row, so a collision partitions by
    // validity window instead of depending on which row arrived first: with a
    // single slot, a genuine pair could be split apart by an unrelated third
    // certificate that happened to occupy the slot.
    let mut by_serial: BTreeMap<(Option<i32>, String), Vec<SearchRow>> = BTreeMap::new();
    let mut no_serial = Vec::new();
    for row in by_id.into_values() {
        let Some(serial) = row.serial.clone() else {
            no_serial.push(row);
            continue;
        };
        let group = by_serial.entry((row.issuer_ca_id, serial)).or_default();
        match group.iter_mut().find(|kept| kept.is_same_cert_as(&row)) {
            Some(kept) => {
                for identity in row.matched_identities {
                    kept.merge_identity(identity);
                }
            }
            None => group.push(row),
        }
    }
    by_serial.into_values().flatten().chain(no_serial).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::Mode;
    use crate::config::{Conn, DEFAULT_DBNAME};

    /// A source pointing somewhere nothing is listening. Any attempt to dial
    /// fails fast and locally, which is what makes "did it dial?" testable
    /// offline — the same address `tests/cli.rs` uses for the connect path.
    fn unreachable_source() -> Source {
        Source::new(Conn {
            host: "127.0.0.1".into(),
            port: 1,
            dbname: DEFAULT_DBNAME.into(),
            user: "guest".into(),
            db_url: None,
        })
    }

    fn cached_row(term: &str) -> RawRow {
        RawRow {
            id: 1,
            issuer_ca_id: Some(1),
            issuer_name: Some("Example CA".into()),
            matched_identity: term.into(),
            common_name: Some(term.into()),
            serial: Some("00".into()),
            not_before: Some(utc(2026, 1, 1)),
            not_after: Some(utc(2026, 12, 31)),
            server_now: chrono::Utc::now(),
        }
    }

    /// The point of the whole cache: a run whose every term is already cached
    /// must finish without opening a connection.
    ///
    /// Proven by pointing the source at a closed port. If `fetch_by_term`
    /// dialled at all it would fail, so a successful call carrying the cached
    /// rows is the assertion — and it needs no network to make it.
    #[tokio::test]
    async fn a_fully_cached_run_never_dials() {
        let dir =
            std::env::temp_dir().join(format!("crt-query-cache-nodial-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cache = Cache::at(dir.clone(), Mode::Enabled, crate::cache::DEFAULT_TTL);

        let sql = "SELECT 1";
        let terms = vec!["a.example".to_string(), "b.example".to_string()];
        let mut source = unreachable_source();
        let target = source.target().unwrap();
        for term in &terms {
            cache.put(
                &Key {
                    target: target.clone(),
                    sql: sql.to_string(),
                    term: term.clone(),
                    params: vec!["365".to_string()],
                },
                &vec![cached_row(term)],
            );
        }

        let limit: i32 = 365;
        let fetched = fetch_by_term(
            &mut source,
            &cache,
            &terms,
            sql,
            &[(&limit, Type::INT4)],
            100,
        )
        .await
        .expect("a fully cached run must not need a connection");

        let rows = fetched.rows;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].matched_identity, "a.example");
        assert_eq!(rows[1].matched_identity, "b.example");
        assert!(
            fetched.saturated.is_empty(),
            "one row per term is nowhere near the cap"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The counterpart: a term that is *not* cached still has to dial, so a
    /// partial cache cannot silently answer with a short result set.
    #[tokio::test]
    async fn an_uncached_term_still_reaches_for_a_connection() {
        let dir = std::env::temp_dir().join(format!("crt-query-cache-miss-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cache = Cache::at(dir.clone(), Mode::Enabled, crate::cache::DEFAULT_TTL);

        let limit: i32 = 365;
        let mut source = unreachable_source();
        let err = fetch_by_term(
            &mut source,
            &cache,
            &["uncached.example".to_string()],
            "SELECT 1",
            &[(&limit, Type::INT4)],
            100,
        )
        .await
        .expect_err("a miss must not be reported as an empty result");
        assert!(
            format!("{err:#}").contains("127.0.0.1:1"),
            "the failure should name the target it could not reach, got: {err:#}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The note exists because a full window and a genuinely small result look
    /// identical in the output, so both halves of the condition carry weight.
    /// All three cases are pinned: dropping either half of the `&&` leaves one
    /// of these failing rather than the whole suite green.
    #[test]
    fn only_a_full_window_that_collapsed_is_worth_a_note() {
        let report = |certs: usize, raw_rows: usize, saturated: bool| Report {
            rows: vec![(); certs],
            raw_rows,
            saturated: if saturated {
                vec!["example.com".to_string()]
            } else {
                Vec::new()
            },
        };
        // 10 identity rows in, 2 certificates out, window full: the reported bug.
        assert!(report(2, 10, true).window_hid_certificates());
        // Full window, nothing collapsed: the caller got the rows they asked for.
        assert!(!report(10, 10, true).window_hid_certificates());
        // Collapsed, but the window never filled: nothing is behind it to find.
        assert!(!report(2, 10, false).window_hid_certificates());
    }

    /// A cache hit has to recognise a full window the same way a fresh fetch
    /// does. The limit is part of the key, so a hit was written under this same
    /// cap and its length means the same thing — but the flag is set on a
    /// separate code path, and skipping it would make the note appear only on
    /// the first run of a query and never again.
    #[tokio::test]
    async fn a_cached_full_window_is_still_reported_as_saturated() {
        let dir = std::env::temp_dir().join(format!("crt-query-cache-sat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cache = Cache::at(dir.clone(), Mode::Enabled, crate::cache::DEFAULT_TTL);

        let sql = "SELECT 1";
        let limit: i32 = 365;
        let mut source = unreachable_source();
        cache.put(
            &Key {
                target: source.target().unwrap(),
                sql: sql.to_string(),
                term: "example.com".to_string(),
                params: vec!["365".to_string()],
            },
            &vec![cached_row("example.com"); 3],
        );

        let fetched = fetch_by_term(
            &mut source,
            &cache,
            &["example.com".to_string()],
            sql,
            &[(&limit, Type::INT4)],
            3,
        )
        .await
        .expect("a cached run must not need a connection");

        assert_eq!(fetched.rows.len(), 3);
        assert_eq!(
            fetched.saturated,
            vec!["example.com".to_string()],
            "three rows against a cap of three is a full window, and the note \
             needs the name of the term that filled it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Snapshots of the three statements this tool sends.
    ///
    /// These are the contract between the SQL and the structs that read it:
    /// every `column("…")` call names an alias defined here, and a projection
    /// or join edit that drops one still compiles and still passes every other
    /// test, then fails at runtime against the real database — which no offline
    /// suite can reach. Re-blessing a snapshot is the prompt to re-check the
    /// readers in that module.
    ///
    /// Verified to catch the mutation they exist for: renaming
    /// `AS matched_identity` in `IDENTITY_QUERY` fails the two snapshots that
    /// embed it and leaves `cert.sql` correctly green.
    #[test]
    fn the_search_statement_matches_its_snapshot() {
        assert_eq!(
            crate::queries::search::sql(),
            include_str!("golden/search.sql"),
            "SEARCH_SQL changed; re-bless src/queries/golden/search.sql and \
             re-check the columns SearchRow reads"
        );
    }

    #[test]
    fn the_expiring_statement_matches_its_snapshot() {
        assert_eq!(
            crate::queries::expiring::sql(),
            include_str!("golden/expiring.sql"),
            "EXPIRING_SQL changed; re-bless src/queries/golden/expiring.sql and \
             re-check the columns ExpiringRow reads"
        );
    }

    #[test]
    fn the_cert_statement_matches_its_snapshot() {
        assert_eq!(
            crate::queries::cert::sql(),
            include_str!("golden/cert.sql"),
            "CERT_SQL changed; re-bless src/queries/golden/cert.sql and \
             re-check the columns CertDetail reads"
        );
    }

    #[test]
    fn the_identity_filter_disables_the_backslash_escape() {
        // Without `ESCAPE ''` every backslash is swallowed and the next
        // character taken literally, so `a\b` searches for `ab` and a trailing
        // backslash cannot match at all — and the run reports "No certificates
        // found", a result people act on. `--help` documents `%` and `_` as the
        // only wildcards; this is what makes that true.
        assert!(
            IDENTITY_QUERY.contains("ILIKE ('%' || $1 || '%') ESCAPE ''"),
            "IDENTITY_QUERY lost its ESCAPE clause:\n{IDENTITY_QUERY}"
        );
    }
    use crate::testutil::utc;

    struct Raw {
        id: i64,
        serial: Option<&'static str>,
        identity: &'static str,
        not_after: DateTime<Utc>,
    }

    fn raw(r: Raw) -> RawRow {
        RawRow {
            id: r.id,
            issuer_ca_id: Some(1),
            issuer_name: Some("Test CA".to_string()),
            matched_identity: r.identity.to_string(),
            common_name: Some("example.com".to_string()),
            serial: r.serial.map(str::to_string),
            not_before: Some(utc(2026, 1, 1)),
            not_after: Some(r.not_after),
            server_now: utc(2026, 2, 1),
        }
    }

    fn pair(id: i64, serial: &'static str, identity: &'static str) -> RawRow {
        raw(Raw {
            id,
            serial: Some(serial),
            identity,
            not_after: utc(2026, 4, 1),
        })
    }

    /// Like [`pair`] but with an explicit issuer.
    ///
    /// The dedupe key is `(issuer_ca_id, serial)` and `is_same_cert_as`
    /// compares only the validity window, so the issuer half is the only thing
    /// keeping two certificates from different CAs that share a serial apart.
    /// Every other fixture here hardcodes `Some(1)`, so nothing exercised it:
    /// reducing the map to a serial-only key left the whole suite green.
    fn pair_from(
        issuer_ca_id: Option<i32>,
        id: i64,
        serial: &'static str,
        identity: &'static str,
    ) -> RawRow {
        RawRow {
            issuer_ca_id,
            ..pair(id, serial, identity)
        }
    }

    #[test]
    fn a_serial_shared_across_two_issuers_stays_two_certificates() {
        // X.509 serials are unique per issuer only, and IDENTITY_QUERY carries
        // no issuer predicate, so one search genuinely returns rows from every
        // CA. Merging these would drop a certificate and leave the survivor
        // advertising an identity it does not carry.
        let rows = to_rows(
            vec![
                pair_from(Some(1), 100, "0a", "one.example.com"),
                pair_from(Some(2), 200, "0a", "two.example.com"),
            ],
            true,
        );
        assert_eq!(
            rows.len(),
            2,
            "two CAs' certificates were collapsed onto one serial"
        );
        for row in &rows {
            assert_eq!(row.matched_identities.len(), 1);
        }
    }

    #[test]
    fn an_unknown_issuer_does_not_collide_with_a_known_one() {
        // The issuer half of the key is Option<i32>, and the LEFT JOIN on `ca`
        // makes None reachable, so it needs its own case.
        let rows = to_rows(
            vec![
                pair_from(None, 100, "0a", "one.example.com"),
                pair_from(Some(1), 200, "0a", "two.example.com"),
            ],
            true,
        );
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn collapses_a_precert_leaf_pair_keeping_the_lowest_id() {
        // The precert and its leaf carry the same serial and validity window.
        let rows = to_rows(
            vec![
                pair(200, "0a", "example.com"),
                pair(100, "0a", "example.com"),
            ],
            true,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, 100);
    }

    #[test]
    fn merges_identities_across_a_collapsed_pair() {
        let rows = to_rows(
            vec![
                pair(100, "0a", "example.com"),
                pair(200, "0a", "www.example.com"),
            ],
            true,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].matched_identities,
            vec!["example.com".to_string(), "www.example.com".to_string()]
        );
    }

    #[test]
    fn merges_identities_within_one_certificate_id() {
        let rows = to_rows(
            vec![
                pair(100, "0a", "example.com"),
                pair(100, "0a", "mail.example.com"),
            ],
            true,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].matched_identities.len(), 2);
    }

    #[test]
    fn does_not_merge_a_serial_collision_between_different_certificates() {
        // Same issuer and serial, different validity: not a precert/leaf pair.
        // Both certificates must survive.
        let rows = to_rows(
            vec![
                raw(Raw {
                    id: 100,
                    serial: Some("0a"),
                    identity: "one.example.com",
                    not_after: utc(2026, 4, 1),
                }),
                raw(Raw {
                    id: 200,
                    serial: Some("0a"),
                    identity: "two.example.com",
                    not_after: utc(2027, 9, 9),
                }),
            ],
            true,
        );
        assert_eq!(
            rows.len(),
            2,
            "a serial collision must not drop a certificate"
        );
        let ids: Vec<_> = rows.iter().map(|r| r.id).collect();
        assert!(ids.contains(&100) && ids.contains(&200));
        // Neither row claims the other's identity.
        for row in &rows {
            assert_eq!(row.matched_identities.len(), 1);
        }
    }

    #[test]
    fn a_collision_does_not_split_a_genuine_pair() {
        // A colliding certificate with the lowest ID must not prevent the real
        // precert/leaf pair behind it from collapsing together.
        let rows = to_rows(
            vec![
                raw(Raw {
                    id: 100,
                    serial: Some("0a"),
                    identity: "other.example.com",
                    not_after: utc(2027, 9, 9),
                }),
                raw(Raw {
                    id: 200,
                    serial: Some("0a"),
                    identity: "example.com",
                    not_after: utc(2026, 4, 1),
                }),
                raw(Raw {
                    id: 300,
                    serial: Some("0a"),
                    identity: "www.example.com",
                    not_after: utc(2026, 4, 1),
                }),
            ],
            true,
        );
        assert_eq!(
            rows.len(),
            2,
            "the pair must collapse despite the collision"
        );
        let pair = rows
            .iter()
            .find(|r| r.id == 200)
            .expect("lowest ID of the pair");
        assert_eq!(pair.matched_identities.len(), 2);
        assert!(rows.iter().any(|r| r.id == 100));
        assert!(
            !rows.iter().any(|r| r.id == 300),
            "leaf should have collapsed"
        );
    }

    #[test]
    fn passes_through_rows_without_a_serial() {
        let rows = to_rows(
            vec![
                raw(Raw {
                    id: 100,
                    serial: None,
                    identity: "a.example.com",
                    not_after: utc(2026, 4, 1),
                }),
                raw(Raw {
                    id: 200,
                    serial: None,
                    identity: "b.example.com",
                    not_after: utc(2026, 4, 1),
                }),
            ],
            true,
        );
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn no_dedupe_preserves_every_raw_row() {
        let rows = to_rows(
            vec![
                pair(100, "0a", "example.com"),
                pair(200, "0a", "example.com"),
            ],
            false,
        );
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn merge_identity_does_not_duplicate() {
        let rows = to_rows(
            vec![
                pair(100, "0a", "example.com"),
                pair(200, "0a", "example.com"),
            ],
            true,
        );
        assert_eq!(rows[0].matched_identities, vec!["example.com".to_string()]);
    }
}
