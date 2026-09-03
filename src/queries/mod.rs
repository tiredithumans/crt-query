pub mod cert;
pub mod expiring;
pub mod search;

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use tokio_postgres::Row;
use tokio_postgres::types::FromSql;

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
    use chrono::NaiveDate;

    fn utc(y: i32, m: u32, d: u32) -> DateTime<Utc> {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
    }

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
