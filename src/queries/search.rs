use std::sync::LazyLock;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio_postgres::types::Type;

use crate::db::Db;
use crate::output::{OutputRecord, fmt_opt, fmt_ts};
use crate::queries::{IDENTITY_QUERY, RawRow, to_rows};

/// Column index of the multi-valued identity field within `cells()`.
const IDENTITIES_COL: usize = 3;

/// `$2` is a look-back in days that bounds which certificates the server-side
/// LIMIT window may be spent on; `0` disables the floor (`--all-history`).
///
/// Without it the LIMIT takes an arbitrary slice of every certificate ever
/// issued for the term, which in practice is the oldest rows — and the
/// client-side sort below then presents that sample as a newest-first list.
static SEARCH_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "{IDENTITY_QUERY}
   AND ($2 = 0
        OR coalesce(x509_notAfter(cai.certificate), 'infinity'::timestamp)
             >= (now() AT TIME ZONE 'UTC') - make_interval(days => $2))
 LIMIT $3"
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

    fn csv_rows(&self) -> Vec<Vec<String>> {
        if self.matched_identities.is_empty() {
            return vec![self.cells()];
        }
        self.matched_identities
            .iter()
            .map(|identity| {
                let mut cells = self.cells();
                cells[IDENTITIES_COL] = identity.clone();
                cells
            })
            .collect()
    }
}

pub async fn run_search(
    db: &Db,
    query: &str,
    valid_since_days: i32,
    limit: i64,
    dedupe: bool,
) -> Result<Vec<SearchRow>> {
    let rows = db
        .query(
            &format!("\"{query}\""),
            SEARCH_SQL.as_str(),
            &[
                (&query, Type::TEXT),
                (&valid_since_days, Type::INT4),
                (&limit, Type::INT8),
            ],
        )
        .await?;
    let raw = rows
        .iter()
        .map(RawRow::from_pg)
        .collect::<Result<Vec<_>>>()?;
    let mut out = to_rows(raw, dedupe);
    out.sort_by_key(|r| std::cmp::Reverse(r.not_before));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn utc(y: i32, m: u32, d: u32) -> DateTime<Utc> {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
    }

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

    #[test]
    fn sql_binds_every_placeholder_it_declares() {
        for p in ["$1", "$2", "$3"] {
            assert!(SEARCH_SQL.contains(p), "missing {p}");
        }
        assert!(!SEARCH_SQL.contains("$4"));
    }

    #[test]
    fn sql_keeps_the_limit_last_and_adds_no_server_side_ordering() {
        assert!(SEARCH_SQL.trim_end().ends_with("LIMIT $3"));
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
