use std::sync::LazyLock;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio_postgres::types::Type;

use crate::cache::Cache;
use crate::db::Source;
use crate::output::{OutputRecord, csv_opt, csv_ts, expand_column, fmt_opt, fmt_ts};
use crate::queries::{IDENTITY_QUERY, RawRow, fetch_by_term, to_rows};

/// Column index of the multi-valued identity field within `cells()`.
const IDENTITIES_COL: usize = 3;

/// `$2` is a look-back in days that bounds which certificates the server-side
/// LIMIT window may be spent on; `0` disables the floor (`--all-history`).
///
/// Without it the LIMIT takes an arbitrary slice of every certificate ever
/// issued for the term, which in practice is the oldest rows — and the
/// client-side sort below then presents that sample as a newest-first list.
///
/// `$3` is `--skip-expired`: a hard floor at the server's own clock, so the
/// window holds only certificates that are valid right now. It is a separate
/// predicate rather than a look-back of zero days because zero is already
/// spoken for by `--all-history`, and it composes with `$2` — the stricter of
/// the two decides, which is always this one when it is set.
static SEARCH_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "{IDENTITY_QUERY}
   AND ($2 = 0
        OR coalesce(x509_notAfter(cai.certificate), 'infinity'::timestamp)
             >= (now() AT TIME ZONE 'UTC') - make_interval(days => $2))
   AND (NOT $3
        OR coalesce(x509_notAfter(cai.certificate), 'infinity'::timestamp)
             >= (now() AT TIME ZONE 'UTC'))
 LIMIT $4"
    )
});

#[derive(Serialize)]
pub struct SearchRow {
    pub id: i64,
    pub issuer_ca_id: Option<i32>,
    pub issuer_name: Option<String>,
    pub matched_identities: Vec<String>,
    pub common_name: Option<String>,
    pub serial: Option<String>,
    pub not_before: Option<DateTime<Utc>>,
    pub not_after: Option<DateTime<Utc>>,
}

impl SearchRow {
    pub fn merge_identity(&mut self, identity: String) {
        if !self.matched_identities.contains(&identity) {
            self.matched_identities.push(identity);
        }
    }

    /// Whether two rows sharing an issuer and serial really are the same
    /// certificate. RFC 6962 gives a precertificate and its leaf the same
    /// validity window, so a mismatch means a serial collision instead.
    pub fn is_same_cert_as(&self, other: &Self) -> bool {
        self.not_before == other.not_before && self.not_after == other.not_after
    }
}

impl From<RawRow> for SearchRow {
    fn from(r: RawRow) -> Self {
        Self {
            id: r.id,
            issuer_ca_id: r.issuer_ca_id,
            issuer_name: r.issuer_name,
            matched_identities: vec![r.matched_identity],
            common_name: r.common_name,
            serial: r.serial,
            not_before: r.not_before,
            not_after: r.not_after,
        }
    }
}

impl OutputRecord for SearchRow {
    fn headers() -> &'static [&'static str] {
        &[
            "crt.sh ID",
            "Issuer CA ID",
            "Issuer",
            "Matched Identities",
            "Common Name",
            "Serial",
            "Not Before (UTC)",
            "Not After (UTC)",
        ]
    }

    fn cells(&self) -> Vec<String> {
        vec![
            self.id.to_string(),
            fmt_opt(self.issuer_ca_id),
            fmt_opt(self.issuer_name.as_deref()),
            self.matched_identities.join(", "),
            fmt_opt(self.common_name.as_deref()),
            fmt_opt(self.serial.as_deref()),
            fmt_ts(self.not_before.as_ref()),
            fmt_ts(self.not_after.as_ref()),
        ]
    }

    fn csv_cells(&self) -> Vec<String> {
        vec![
            self.id.to_string(),
            csv_opt(self.issuer_ca_id),
            csv_opt(self.issuer_name.as_deref()),
            self.matched_identities.join(", "),
            csv_opt(self.common_name.as_deref()),
            csv_opt(self.serial.as_deref()),
            csv_ts(self.not_before.as_ref()),
            csv_ts(self.not_after.as_ref()),
        ]
    }

    fn csv_rows(&self) -> Vec<Vec<String>> {
        expand_column(self.csv_cells(), IDENTITIES_COL, &self.matched_identities)
    }
}

/// Search one or more terms and merge the results into a single list.
///
/// One statement per term, in sequence — see [`fetch_by_term`] for why.
pub async fn run_search(
    source: &mut Source,
    cache: &Cache,
    queries: &[String],
    valid_since_days: i32,
    skip_expired: bool,
    limit: i64,
    dedupe: bool,
) -> Result<Vec<SearchRow>> {
    let raw = fetch_by_term(
        source,
        cache,
        queries,
        SEARCH_SQL.as_str(),
        &[
            (&valid_since_days, Type::INT4),
            (&skip_expired, Type::BOOL),
            (&limit, Type::INT8),
        ],
    )
    .await?;
    Ok(assemble_search(raw, dedupe))
}

/// Turn the rows every statement returned into the finished, sorted list.
///
/// Split out of `run_search` for the same reason as
/// [`crate::queries::expiring::assemble_expiring`]: everything above it needs a
/// database and nothing here does, so the newest-first ordering — which is the
/// whole reason the client-side sort exists — had no seam a test could reach.
/// Deleting the sort compiled clean and left the suite green.
///
/// Dedup runs over the merged rows, so a certificate matching two of the terms
/// appears once, carrying both matched identities.
fn assemble_search(raw: Vec<RawRow>, dedupe: bool) -> Vec<SearchRow> {
    let mut out = to_rows(raw, dedupe);
    // A certificate whose notBefore crt.sh could not parse has no place in a
    // newest-first order, so it goes last. `None` sorts before `Some`, which
    // `Reverse` alone would put at the head of the list, ahead of every
    // certificate with a real date.
    out.sort_by_key(|r| (r.not_before.is_none(), std::cmp::Reverse(r.not_before)));
    out
}

/// The exact statement this module sends, for the golden-file test in
/// `queries::tests`. Reading it through one accessor keeps the snapshot tied
/// to what actually runs.
#[cfg(test)]
pub(crate) fn sql() -> &'static str {
    SEARCH_SQL.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{Cache, Key, Mode};
    use crate::config::{Conn, DEFAULT_DBNAME};

    /// The whole feature, through the function `main` actually calls.
    ///
    /// `a_fully_cached_run_never_dials` proves the mechanism inside
    /// `fetch_by_term`; this proves `run_search` builds the same key on the way
    /// in that it wrote on the way out — with the real `SEARCH_SQL` and the
    /// real bind parameters, so a change to either shows up here rather than as
    /// a cache that silently never hits.
    ///
    /// The source points at a closed port, so a connection attempt would fail
    /// the test. Offline, and nothing leaves the machine.
    #[tokio::test]
    async fn a_cached_search_is_served_without_a_connection() {
        let dir =
            std::env::temp_dir().join(format!("crt-query-cache-search-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cache = Cache::at(dir.clone(), Mode::Enabled, crate::cache::DEFAULT_TTL);

        let mut source = Source::new(Conn {
            host: "127.0.0.1".into(),
            port: 1,
            dbname: DEFAULT_DBNAME.into(),
            user: "guest".into(),
            db_url: None,
        });

        // The parameters `run_search` binds below, rendered exactly as
        // `fetch_by_term` renders them.
        let (valid_since, skip_expired, limit) = (365i32, false, 100i64);
        cache.put(
            &Key {
                target: source.target().unwrap(),
                sql: sql().to_string(),
                term: "example.com".to_string(),
                params: vec![
                    format!("{valid_since:?}"),
                    format!("{skip_expired:?}"),
                    format!("{limit:?}"),
                ],
            },
            &vec![RawRow {
                id: 42,
                issuer_ca_id: Some(7),
                issuer_name: Some("Example CA".into()),
                matched_identity: "example.com".into(),
                common_name: Some("example.com".into()),
                serial: Some("0a".into()),
                not_before: Some(utc(2026, 1, 1)),
                not_after: Some(utc(2026, 12, 31)),
                server_now: Utc::now(),
            }],
        );

        let rows = run_search(
            &mut source,
            &cache,
            &["example.com".to_string()],
            valid_since,
            skip_expired,
            limit,
            true,
        )
        .await
        .expect("a cached search must not need a connection");

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, 42);
        assert_eq!(rows[0].matched_identities, vec!["example.com".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// README: "--json … including `[]` for an empty result". Serialising an
    /// empty Vec is what makes that true, and a future switch to an object
    /// wrapper would break every consumer silently.
    #[test]
    fn an_empty_search_serialises_as_an_empty_json_array() {
        let rows: Vec<SearchRow> = Vec::new();
        assert_eq!(serde_json::to_string(&rows).unwrap(), "[]");
    }
    use crate::testutil::utc;

    fn row(not_after: Option<DateTime<Utc>>) -> SearchRow {
        SearchRow {
            id: 1,
            issuer_ca_id: None,
            issuer_name: None,
            matched_identities: vec!["example.com".to_string()],
            common_name: None,
            serial: None,
            not_before: Some(utc(2026, 1, 1)),
            not_after,
        }
    }

    fn raw_row(id: i64, not_before: DateTime<Utc>) -> RawRow {
        RawRow {
            id,
            issuer_ca_id: Some(1),
            issuer_name: None,
            matched_identity: "example.com".to_string(),
            common_name: None,
            serial: Some(format!("{id:02x}")),
            not_before: Some(not_before),
            not_after: Some(utc(2027, 1, 1)),
            server_now: utc(2026, 2, 1),
        }
    }

    /// The newest-first order is the whole reason the client-side sort exists —
    /// the statement carries no ORDER BY so LIMIT can terminate early. It lived
    /// inside an async fn and so was unreachable offline: deleting it compiled
    /// clean and passed.
    #[test]
    fn assembled_rows_are_ordered_newest_first() {
        let rows = assemble_search(
            vec![
                raw_row(1, utc(2026, 1, 1)),
                raw_row(2, utc(2026, 6, 1)),
                raw_row(3, utc(2026, 3, 1)),
            ],
            true,
        );
        let order: Vec<i64> = rows.iter().map(|r| r.id).collect();
        assert_eq!(order, vec![2, 3, 1], "search must present newest first");
    }

    /// `None` orders before `Some`, so under a plain `Reverse` a certificate
    /// with no parseable notBefore led the list, ahead of every certificate
    /// with a real date. Unknown is not newest; it goes last.
    #[test]
    fn a_row_without_a_not_before_sorts_after_every_dated_row() {
        let rows = assemble_search(
            vec![
                RawRow {
                    not_before: None,
                    ..raw_row(1, utc(2026, 1, 1))
                },
                raw_row(2, utc(2026, 1, 1)),
                raw_row(3, utc(2026, 6, 1)),
            ],
            true,
        );
        let order: Vec<i64> = rows.iter().map(|r| r.id).collect();
        assert_eq!(order, vec![3, 2, 1], "an undated row must come last");
    }

    #[test]
    fn sql_binds_every_placeholder_it_declares() {
        for p in ["$1", "$2", "$3", "$4"] {
            assert!(SEARCH_SQL.contains(p), "missing {p}");
        }
        assert!(!SEARCH_SQL.contains("$5"));
    }

    #[test]
    fn skip_expired_floors_the_window_at_the_server_clock() {
        // Guarded because the predicate must compare against the server's own
        // now(), not a client-side timestamp: the same rule that keeps
        // `expiring --skip-expired` from surfacing an EXPIRED row.
        assert!(SEARCH_SQL.contains("NOT $3"));
        assert!(SEARCH_SQL.contains("AND (NOT $3\n        OR coalesce"));
    }

    #[test]
    fn sql_keeps_the_limit_last_and_adds_no_server_side_ordering() {
        assert!(SEARCH_SQL.trim_end().ends_with("LIMIT $4"));
        assert!(!SEARCH_SQL.contains("ORDER BY"));
    }

    #[test]
    fn headers_and_cells_agree_in_arity() {
        assert_eq!(SearchRow::headers().len(), row(None).cells().len());
    }

    #[test]
    fn identities_col_points_at_the_identities_column() {
        assert_eq!(SearchRow::headers()[IDENTITIES_COL], "Matched Identities");
    }

    #[test]
    fn null_columns_render_as_a_dash() {
        let cells = row(None).cells();
        assert_eq!(cells[1], "-");
        assert_eq!(cells[7], "-");
    }

    #[test]
    fn a_precert_and_its_leaf_share_a_validity_window() {
        let a = row(Some(utc(2026, 4, 1)));
        let b = row(Some(utc(2026, 4, 1)));
        let c = row(Some(utc(2027, 4, 1)));
        assert!(a.is_same_cert_as(&b));
        assert!(!a.is_same_cert_as(&c));
    }
}
