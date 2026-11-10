use std::sync::LazyLock;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio_postgres::types::Type;

use crate::cache::Cache;
use crate::db::Source;
use crate::output::{OutputRecord, csv_opt, fmt_opt};
use crate::queries::search::SearchRow;
use crate::queries::{IDENTITY_QUERY, RawRow, Report, drop_lapsed, fetch_by_term, to_rows};

const MILLIS_PER_DAY: i64 = 86_400_000;

/// One query serves both modes. `$2` is the look-ahead and `$3` the look-back,
/// so `--skip-expired` is exactly a zero-day look-back and needs no second
/// statement to drift out of sync with this one.
///
/// The look-back matters: with only an upper bound, every certificate that ever
/// expired satisfies the predicate, and since LIMIT terminates early without an
/// ORDER BY the window fills with ancient rows before reaching anything close
/// to expiring.
static EXPIRING_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "{IDENTITY_QUERY}
   AND x509_notAfter(cai.certificate)
         <= (now() AT TIME ZONE 'UTC') + make_interval(days => $2)
   AND x509_notAfter(cai.certificate)
         >= (now() AT TIME ZONE 'UTC') - make_interval(days => $3)
 LIMIT $4"
    )
});

#[derive(Serialize)]
pub struct ExpiringRow {
    #[serde(flatten)]
    pub cert: SearchRow,
    /// Whole days until expiry, floored: negative once expired, `0` only for a
    /// certificate expiring within the next 24 hours.
    pub days_left: Option<i64>,
    pub status: String,
}

impl ExpiringRow {
    fn new(cert: SearchRow, now: DateTime<Utc>) -> Self {
        // Floor rather than truncate toward zero, so a certificate that died
        // two hours ago reports -1 instead of sharing 0 with one that has 23
        // hours left. `days_left` is then usable on its own in JSON and CSV.
        //
        // Milliseconds, not seconds: X.509 notAfter has second precision but
        // the server clock has microsecond precision, so within the first
        // second after expiry a seconds-truncating difference rounds to 0 and
        // would contradict the EXPIRED status.
        let days_left = cert
            .not_after
            .map(|t| (t - now).num_milliseconds().div_euclid(MILLIS_PER_DAY));
        let status = match (cert.not_after, days_left) {
            (None, _) | (_, None) => "-".to_string(),
            (Some(t), Some(_)) if t < now => "EXPIRED".to_string(),
            (Some(_), Some(0)) => "EXPIRES TODAY".to_string(),
            (Some(_), Some(d)) => format!("EXPIRES IN {d}d"),
        };
        Self {
            cert,
            days_left,
            status,
        }
    }
}

impl OutputRecord for ExpiringRow {
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
            "Days Left",
            "Status",
        ]
    }

    fn cells(&self) -> Vec<String> {
        let mut cells = self.cert.cells();
        cells.push(fmt_opt(self.days_left));
        cells.push(self.status.clone());
        cells
    }

    fn csv_cells(&self) -> Vec<String> {
        let mut cells = self.cert.csv_cells();
        cells.push(csv_opt(self.days_left));
        cells.push(self.status.clone());
        cells
    }

    fn csv_rows(&self) -> Vec<Vec<String>> {
        self.cert
            .csv_rows()
            .into_iter()
            .map(|mut cells| {
                // Empty, not a dash: days_left is the column a script reads as
                // a number, and one placeholder types the whole column as text.
                cells.push(csv_opt(self.days_left));
                cells.push(self.status.clone());
                cells
            })
            .collect()
    }
}

/// What an `expiring` run was asked for, flags already resolved.
#[derive(Debug, Clone, Copy)]
pub struct Asked {
    /// Look-ahead, in days.
    pub within: i32,
    /// Look-back, in days; `0` for `--skip-expired`.
    pub since_expired: i32,
    /// Identity rows per domain.
    pub limit: i64,
    pub dedupe: bool,
    /// Leave out certificates a longer-lived one has replaced.
    pub ignore_renewed: bool,
}

/// Check one or more domains and merge the results into a single report.
///
/// One statement per domain, in sequence — see [`fetch_by_term`] for why.
pub async fn run_expiring(
    source: &mut Source,
    cache: &Cache,
    domains: &[String],
    asked: Asked,
) -> Result<Report<ExpiringRow>> {
    let Asked {
        within,
        since_expired,
        limit,
        dedupe,
        ignore_renewed,
    } = asked;
    let fetched = fetch_by_term(
        source,
        cache,
        domains,
        EXPIRING_SQL.as_str(),
        &[
            (&within, Type::INT4),
            (&since_expired, Type::INT4),
            (&limit, Type::INT8),
        ],
        limit,
    )
    .await?;
    let mut raw = fetched.rows;
    // A cached entry holds the window as it stood when it was written; see
    // `drop_lapsed` for what that used to let through.
    drop_lapsed(&mut raw, Some(since_expired));
    let raw_rows = raw.len();
    let mut rows = assemble_expiring(raw, dedupe);
    let mut cached = fetched.cached;
    let mut renewed = 0;
    let mut renewals_unseen = Vec::new();
    // Nothing listed, nothing to excuse: skip the second statement.
    if ignore_renewed && !rows.is_empty() {
        let later = fetch_later(source, cache, domains, within, limit).await?;
        let before = rows.len();
        let covered = Covered::by(&later.rows);
        rows.retain(|row| !covered.all_of(&row.cert.matched_identities));
        renewed = before - rows.len();
        renewals_unseen = later.saturated;
        cached = match (cached, later.cached) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
    }
    Ok(Report {
        rows,
        raw_rows,
        saturated: fetched.saturated,
        empty: fetched.empty,
        cached,
        renewed,
        renewals_unseen,
    })
}

/// How far ahead the look for a replacement certificate reaches: the widest
/// window the statement's `make_interval` is ever given.
const FAR_AHEAD_DAYS: i32 = 36_500;

/// The certificates for `domains` that outlive the `--within` window.
///
/// The same statement as the report itself, with the window moved: `$2`
/// reaches as far ahead as it goes, and a *negative* look-back puts the floor
/// at `now() + within` instead of behind `now()`. So there is no second
/// statement to keep in step with the first, no new golden file, and the
/// `LIMIT` window is still bounded by a validity predicate on both sides.
async fn fetch_later(
    source: &mut Source,
    cache: &Cache,
    domains: &[String],
    within: i32,
    limit: i64,
) -> Result<crate::queries::Fetched> {
    let floor = -within;
    let mut later = fetch_by_term(
        source,
        cache,
        domains,
        EXPIRING_SQL.as_str(),
        &[
            (&FAR_AHEAD_DAYS, Type::INT4),
            (&floor, Type::INT4),
            (&limit, Type::INT8),
        ],
        limit,
    )
    .await?;
    // A cached entry's floor has moved on with the clock, like any other.
    drop_lapsed(&mut later.rows, Some(floor));
    Ok(later)
}

/// The names that some longer-lived certificate carries.
///
/// What `--ignore-renewed` asks of each listed certificate: is every name it
/// matched on here? `expiring` selects by notAfter alone, so it lists a
/// certificate for its whole last month whether or not its replacement was
/// issued weeks ago — which, for anything renewed automatically, is always.
/// `--fail-on-expiring` then exited 4 for a healthy domain, every day.
///
/// Only ever positive evidence. A name is covered because a certificate
/// carrying it was seen; one that went unseen — the look for newer
/// certificates has a `--limit` too — leaves its certificate listed. The
/// failure is an alert that was not needed, never one that was not raised.
struct Covered(std::collections::BTreeSet<String>);

impl Covered {
    fn by(later: &[RawRow]) -> Self {
        Self(
            later
                .iter()
                .map(|row| row.matched_identity.to_lowercase())
                .collect(),
        )
    }

    /// Whether `name` is on a longer-lived certificate, itself or under a
    /// wildcard one label up: `*.example.com` covers `www.example.com`, as it
    /// does in a TLS handshake, and not `a.www.example.com`.
    fn has(&self, name: &str) -> bool {
        let name = name.to_lowercase();
        if self.0.contains(&name) {
            return true;
        }
        name.split_once('.')
            .is_some_and(|(_, parent)| self.0.contains(&format!("*.{parent}")))
    }

    /// Every one of `names`. A certificate with no name to check is not
    /// excused by default.
    fn all_of(&self, names: &[String]) -> bool {
        !names.is_empty() && names.iter().all(|name| self.has(name))
    }
}

/// Turn the rows every statement returned into the finished, sorted list.
///
/// Split out from `run_expiring` because everything above it needs a database
/// and everything here does not: this is where the one non-obvious rule in
/// this module lives, and a rule with no test is a rule that comes back.
fn assemble_expiring(raw: Vec<RawRow>, dedupe: bool) -> Vec<ExpiringRow> {
    // The same clock the server used to choose the window decides the labels,
    // so --skip-expired can never surface a row marked EXPIRED — for rows out
    // of the cache too, now that `run_expiring` re-applies the floor against
    // their replayed clock before they get here. Across
    // several statements that means the *earliest* clock: every row satisfied
    // its own query's `now()`, which is at or after this one, so labelling
    // against the earliest can only ever be conservative.
    let now = raw
        .iter()
        .map(|r| r.server_now)
        .min()
        .unwrap_or_else(Utc::now);
    // Dedup runs over the merged rows, so a certificate covering two of the
    // requested domains appears once, carrying both matched identities.
    let mut out: Vec<ExpiringRow> = to_rows(raw, dedupe)
        .into_iter()
        .map(|r| ExpiringRow::new(r, now))
        .collect();
    // Soonest first. A row with no parseable notAfter — reachable only with
    // `--no-dedupe` quirks or a future predicate change, since the SQL window
    // excludes a NULL — is not "soonest"; `None` sorts before `Some`, so it
    // has to be sent to the end explicitly.
    out.sort_by_key(|r| (r.cert.not_after.is_none(), r.cert.not_after));
    out
}

/// The exact statement this module sends, for the golden-file test in
/// `queries::tests`. Reading it through one accessor keeps the snapshot tied
/// to what actually runs.
#[cfg(test)]
pub(crate) fn sql() -> &'static str {
    EXPIRING_SQL.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, NaiveDate};

    fn now() -> DateTime<Utc> {
        NaiveDate::from_ymd_opt(2026, 6, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc()
    }

    fn at(offset: Duration) -> ExpiringRow {
        let cert = SearchRow {
            id: 1,
            issuer_ca_id: Some(1),
            issuer_name: None,
            matched_identities: vec!["example.com".to_string()],
            common_name: None,
            serial: Some("0a".to_string()),
            not_before: Some(now() - Duration::days(90)),
            not_after: Some(now() + offset),
        };
        ExpiringRow::new(cert, now())
    }

    fn raw(id: i64, not_after: DateTime<Utc>, server_now: DateTime<Utc>) -> RawRow {
        RawRow {
            id,
            issuer_ca_id: Some(1),
            issuer_name: None,
            matched_identity: "example.com".to_string(),
            common_name: None,
            serial: Some(format!("{id:02x}")),
            not_before: Some(server_now - Duration::days(90)),
            not_after: Some(not_after),
            server_now,
        }
    }

    /// `expiring` sends one statement per domain, each stamping its own
    /// `now()`; labelling against anything but the *earliest* can mark a row
    /// EXPIRED that its own query accepted as live — what `--skip-expired`
    /// promises cannot happen. The module's one non-obvious rule, and it was
    /// previously unreachable from a test.
    #[test]
    fn labels_come_from_the_earliest_server_clock_across_statements() {
        let early = now();
        let late = now() + Duration::seconds(10);
        // Expires between the two clocks: live by the earlier, dead by the later.
        let boundary = now() + Duration::seconds(5);

        let rows = assemble_expiring(
            vec![
                raw(1, boundary, late),
                raw(2, boundary + Duration::days(30), early),
            ],
            true,
        );

        let boundary_row = rows.iter().find(|r| r.cert.id == 1).expect("row 1 present");
        assert_ne!(
            boundary_row.status, "EXPIRED",
            "labelled EXPIRED against the later clock; the earliest statement's \
             now() still had this certificate live"
        );
    }

    fn later(identities: &[&str]) -> Covered {
        let n = now();
        let rows: Vec<RawRow> = identities
            .iter()
            .map(|identity| RawRow {
                matched_identity: (*identity).to_string(),
                ..raw(9, n + Duration::days(80), n)
            })
            .collect();
        Covered::by(&rows)
    }

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(ToString::to_string).collect()
    }

    /// A certificate is renewed when every name it matched lives on; one
    /// name left behind is the whole reason to keep the alert.
    #[test]
    fn a_certificate_is_renewed_only_when_every_name_on_it_is_covered() {
        let covered = later(&["example.com", "WWW.example.com"]);
        assert!(covered.all_of(&names(&["example.com"])));
        assert!(
            covered.all_of(&names(&["Example.com", "www.example.com"])),
            "names compare without regard to case"
        );
        assert!(
            !covered.all_of(&names(&["example.com", "mail.example.com"])),
            "mail.example.com has no replacement and must stay listed"
        );
        assert!(!covered.all_of(&[]), "nothing to check is not a renewal");
        assert!(!later(&[]).all_of(&names(&["example.com"])));
    }

    #[test]
    fn a_wildcard_covers_one_label_and_no_more() {
        let covered = later(&["*.example.com"]);
        assert!(covered.has("www.example.com"));
        assert!(covered.has("*.example.com"));
        assert!(!covered.has("a.www.example.com"));
        assert!(!covered.has("example.com"), "the apex is not under it");
    }

    /// The look for replacements re-sends the report's own statement with the
    /// window moved, so its floor is a negative look-back. That only works
    /// while both bounds are still in the text it sends.
    #[test]
    fn the_look_for_replacements_reuses_the_bounded_statement() {
        assert!(EXPIRING_SQL.contains("- make_interval(days => $3)"));
        assert!(EXPIRING_SQL.contains("+ make_interval(days => $2)"));
        // And a cached replacement that has slipped back inside the window
        // since it was stored is no longer one.
        let n = now();
        let mut rows = vec![
            raw(1, n + Duration::days(29), n),
            raw(2, n + Duration::days(31), n),
        ];
        drop_lapsed(&mut rows, Some(-30));
        assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn an_empty_result_assembles_without_reaching_for_a_clock() {
        assert!(assemble_expiring(Vec::new(), true).is_empty());
    }

    #[test]
    fn assembled_rows_are_ordered_by_expiry() {
        let n = now();
        let rows = assemble_expiring(
            vec![
                raw(1, n + Duration::days(30), n),
                raw(2, n + Duration::days(3), n),
                raw(3, n + Duration::days(10), n),
            ],
            true,
        );
        let order: Vec<i64> = rows.iter().map(|r| r.cert.id).collect();
        assert_eq!(order, vec![2, 3, 1]);
    }

    /// `None` orders before `Some`, so a row with no notAfter used to head a
    /// soonest-first report, above the certificate actually about to expire.
    #[test]
    fn a_row_without_a_not_after_sorts_after_every_dated_row() {
        let n = now();
        let rows = assemble_expiring(
            vec![
                RawRow {
                    not_after: None,
                    ..raw(1, n, n)
                },
                raw(2, n + Duration::days(30), n),
                raw(3, n + Duration::days(3), n),
            ],
            true,
        );
        let order: Vec<i64> = rows.iter().map(|r| r.cert.id).collect();
        assert_eq!(order, vec![3, 2, 1], "an undated row must come last");
        assert_eq!(rows[2].status, "-");
    }

    /// `--json` is a machine contract: `days_left` and `status` sit beside the
    /// certificate's own fields rather than under a nested object, and that is
    /// `#[serde(flatten)]` doing it. Dropping the attribute reshapes every
    /// document this tool has ever emitted, and nothing else would notice.
    #[test]
    fn the_expiring_json_document_stays_flat() {
        let value = serde_json::to_value(at(Duration::days(5))).unwrap();
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
                "days_left",
                "id",
                "issuer_ca_id",
                "issuer_name",
                "matched_identities",
                "not_after",
                "not_before",
                "serial",
                "status",
            ],
            "the --json shape changed; #[serde(flatten)] on ExpiringRow::cert \
             is what keeps these ten keys at the top level"
        );
    }

    #[test]
    fn json_timestamps_carry_an_explicit_utc_marker() {
        let value = serde_json::to_value(at(Duration::days(5))).unwrap();
        let not_after = value["not_after"].as_str().expect("not_after is a string");
        assert!(
            not_after.ends_with('Z'),
            "README promises JSON timestamps carry an explicit Z: {not_after}"
        );
    }

    #[test]
    fn a_certificate_that_expired_hours_ago_reports_negative_days() {
        let row = at(-Duration::hours(2));
        assert_eq!(row.status, "EXPIRED");
        assert_eq!(
            row.days_left,
            Some(-1),
            "must not share days_left 0 with a certificate that is still valid"
        );
    }

    #[test]
    fn a_certificate_expiring_within_a_day_reports_zero() {
        let row = at(Duration::hours(23));
        assert_eq!(row.status, "EXPIRES TODAY");
        assert_eq!(row.days_left, Some(0));
    }

    #[test]
    fn a_certificate_expiring_in_exactly_one_day() {
        let row = at(Duration::days(1));
        assert_eq!(row.status, "EXPIRES IN 1d");
        assert_eq!(row.days_left, Some(1));
    }

    #[test]
    fn the_expiry_boundary_is_exact() {
        let just_expired = at(-Duration::seconds(1));
        assert_eq!(just_expired.status, "EXPIRED");
        assert_eq!(just_expired.days_left, Some(-1));

        let exactly_now = at(Duration::zero());
        assert_eq!(exactly_now.status, "EXPIRES TODAY");
        assert_eq!(exactly_now.days_left, Some(0));
    }

    #[test]
    fn status_and_days_left_never_contradict_within_the_first_second() {
        // notAfter has second precision, the server clock has microsecond
        // precision, so this sub-second gap is genuinely reachable.
        let row = at(-Duration::milliseconds(500));
        assert_eq!(row.status, "EXPIRED");
        assert_eq!(
            row.days_left,
            Some(-1),
            "EXPIRED must never report a non-negative days_left"
        );
    }

    #[test]
    fn long_expired_certificates_stay_negative() {
        assert_eq!(at(-Duration::days(1856)).days_left, Some(-1856));
    }

    #[test]
    fn a_missing_not_after_renders_a_dash() {
        let cert = SearchRow {
            id: 1,
            issuer_ca_id: None,
            issuer_name: None,
            matched_identities: vec!["example.com".to_string()],
            common_name: None,
            serial: None,
            not_before: None,
            not_after: None,
        };
        let row = ExpiringRow::new(cert, now());
        assert_eq!(row.status, "-");
        assert_eq!(row.days_left, None);
        assert_eq!(row.cells().last().unwrap(), "-");
    }

    /// `cells()` delegates its first eight values to SearchRow while
    /// `headers()` retypes all eight by hand. The arity tests compare lengths
    /// only, so a rename desynchronises the two tables silently — and it is not
    /// cosmetic: `constrain_columns` matches on header TEXT, so one side also
    /// loses its layout constraint, and `headers()` is the CSV header row, so
    /// the two subcommands' machine output disagrees on a column name.
    #[test]
    fn expiring_extends_the_search_columns_rather_than_restating_them() {
        assert!(
            ExpiringRow::headers().starts_with(SearchRow::headers()),
            "ExpiringRow::headers() no longer starts with SearchRow's, but \
             cells() still delegates to it:\n  search:   {:?}\n  expiring: {:?}",
            SearchRow::headers(),
            ExpiringRow::headers()
        );
    }

    #[test]
    fn headers_and_cells_agree_in_arity() {
        assert_eq!(
            ExpiringRow::headers().len(),
            at(Duration::days(5)).cells().len()
        );
    }

    #[test]
    fn csv_expands_identities_and_keeps_the_trailing_columns() {
        let mut row = at(Duration::days(5));
        row.cert.merge_identity("www.example.com".to_string());
        let rows = row.csv_rows();
        assert_eq!(rows.len(), 2);
        for r in &rows {
            assert_eq!(r.len(), ExpiringRow::headers().len());
            assert_eq!(r[9], "EXPIRES IN 5d");
        }
        assert_eq!(rows[0][3], "example.com");
        assert_eq!(rows[1][3], "www.example.com");
    }

    #[test]
    fn sql_binds_every_placeholder_and_bounds_the_window_on_both_sides() {
        for p in ["$1", "$2", "$3", "$4"] {
            assert!(EXPIRING_SQL.contains(p), "missing {p}");
        }
        assert!(EXPIRING_SQL.contains("+ make_interval(days => $2)"));
        assert!(EXPIRING_SQL.contains("- make_interval(days => $3)"));
        assert!(EXPIRING_SQL.trim_end().ends_with("LIMIT $4"));
        assert!(!EXPIRING_SQL.contains("ORDER BY"));
    }
}
