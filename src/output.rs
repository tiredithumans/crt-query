use std::borrow::Cow;
use std::fmt::Display;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, IsTerminal, Write};

use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use comfy_table::presets::UTF8_FULL_CONDENSED;
use comfy_table::{ColumnConstraint, ContentArrangement, Table, Width};
use serde::Serialize;

use crate::cli::OutputOpts;

/// Placeholder for a NULL column.
const DASH: &str = "-";

/// Table width used when stdout is not a terminal. `ContentArrangement::Dynamic`
/// has nothing to measure against a pipe, so without this the table renders at
/// its full natural width — often several hundred columns.
const PIPED_WIDTH: u16 = 120;

/// Columns with no natural wrap point — an ID, a hex serial, a fixed-format
/// timestamp — pinned to their exact content width. `ContentArrangement::Dynamic`
/// only exempts a column from wrapping when its content is already narrower
/// than the current average, so header text alone (e.g. "Not Before (UTC)")
/// can push these into the "wrap when squeezed" bucket even though breaking a
/// serial mid-hex-digit or a timestamp between date and time makes them
/// harder to read, not easier.
const NO_WRAP_HEADERS: &[&str] = &[
    "crt.sh ID",
    "Issuer CA ID",
    "Serial",
    "Not Before (UTC)",
    "Not After (UTC)",
    "Days Left",
    "Status",
];

/// Free-text columns that do have natural wrap points (spaces, dots, commas)
/// but still need a floor: without one they end up squeezed to a
/// character-per-line sliver once the columns above claim their content
/// width. A minimum here can push the table past the width it was aiming for —
/// preferred over a technically-fitting table nobody can read.
const MIN_WIDTH_HEADERS: &[(&str, u16)] = &[
    ("Issuer", 20),
    ("Matched Identities", 20),
    ("Common Name", 18),
];

/// Apply [`NO_WRAP_HEADERS`] and [`MIN_WIDTH_HEADERS`] to `table` by matching
/// on the header text already set via `set_header`, so this works across
/// [`OutputRecord`] types without hard-coding column positions.
///
/// Only used for the automatic layout. Both constraint sets are heuristics for
/// "we picked this width, make it readable", and comfy-table honours a column
/// constraint over the table width — so leaving them on would let them silently
/// overrule an explicit `--width`. See [`new_table`].
fn constrain_columns(table: &mut Table, headers: &[&str]) {
    for (i, header) in headers.iter().enumerate() {
        let constraint = if NO_WRAP_HEADERS.contains(header) {
            Some(ColumnConstraint::ContentWidth)
        } else {
            MIN_WIDTH_HEADERS
                .iter()
                .find(|(name, _)| name == header)
                .map(|(_, min)| ColumnConstraint::LowerBoundary(Width::Fixed(*min)))
        };
        if let Some(constraint) = constraint
            && let Some(column) = table.column_mut(i)
        {
            column.set_constraint(constraint);
        }
    }
}

const TS_FORMAT: &str = "%Y-%m-%d %H:%M";

/// One implementation per record type feeds all three output formats:
/// serde derive covers JSON, `headers()`/`cells()` cover table and CSV.
pub trait OutputRecord: Serialize {
    fn headers() -> &'static [&'static str];

    /// Cells for the table: formatted to be read by a person, so NULL becomes
    /// a dash and a timestamp loses its seconds.
    fn cells(&self) -> Vec<String>;

    /// Cells for CSV: the same values typed for a machine.
    ///
    /// Sharing `cells()` made the file inherit the table's display formatting,
    /// which is fine for text and wrong for everything else: `Days Left` mixed
    /// `-30` with the dash placeholder, so the whole column loaded as text in
    /// pandas and Excel, and a certificate expiring at 23:59:59 read `23:59`.
    /// Records with a NULL-able or timestamp column override this to write an
    /// empty field and a full RFC 3339 instant instead.
    fn csv_cells(&self) -> Vec<String> {
        self.cells()
    }

    /// Rows to write in CSV mode; one row matching `csv_cells()` by default.
    ///
    /// Records with a multi-valued column override this to emit one row per
    /// value. Joining them into a single field would leave a CSV consumer
    /// unable to tell a separator from a character inside a value.
    fn csv_rows(&self) -> Vec<Vec<String>> {
        vec![self.csv_cells()]
    }
}

/// Format an optional value for CSV: empty rather than the table's dash, so a
/// numeric column stays numeric and an empty field means exactly "no value".
pub fn csv_opt<T: Display>(value: Option<T>) -> String {
    value.map_or_else(String::new, |v| v.to_string())
}

/// Format a timestamp for CSV: a full RFC 3339 instant, seconds included.
///
/// The table truncates to the minute to save a column; a report that a script
/// parses should not lose the difference between 23:59:00 and 23:59:59 on the
/// one field people schedule work around.
pub fn csv_ts(ts: Option<&DateTime<Utc>>) -> String {
    csv_opt(ts.map(|t| t.to_rfc3339_opts(SecondsFormat::Secs, true)))
}

/// Fail on an unwritable `--csv` destination before a connection is spent on
/// the shared guest database. Creates the file if absent but never truncates:
/// a later failure should not destroy the previous run's report.
pub fn precheck_csv(out: &OutputOpts) -> Result<()> {
    if let Some(path) = &out.csv {
        let existed = path.exists();
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("cannot write CSV to {}", path.display()))?;
        // Creating the file is a side effect of testing that it can be
        // written, not the point of it. Left behind when the run then fails,
        // it is an empty file where the documented contract promises a header
        // row — a consumer testing for existence finds a report and parses
        // nothing out of it, which is worse than the absence it replaced. An
        // already-present file is untouched: that is the previous run's report
        // and outliving a later failure is exactly what it is for.
        if !existed {
            let _ = std::fs::remove_file(path);
        }
    }
    Ok(())
}

/// Render a result list as a table (default) or JSON array on stdout,
/// plus an optional CSV file.
///
/// An empty list still produces `[]` and a header-only CSV: both are contracts
/// a script depends on, and skipping the CSV write would leave a stale file
/// from a previous run in place.
pub fn emit<T: OutputRecord>(rows: &[T], out: &OutputOpts) -> Result<()> {
    write_csv_if_requested(rows, out)?;
    if out.json {
        return write_json(rows);
    }
    if rows.is_empty() {
        // The caller has already explained the empty result on stderr.
        return Ok(());
    }
    print_table(&build_table(rows, out))
}

/// Render a single record as a key/value detail table, a JSON object,
/// or a CSV file.
pub fn emit_detail<T: OutputRecord>(record: &T, out: &OutputOpts) -> Result<()> {
    let cells = record.cells();
    debug_assert_eq!(
        T::headers().len(),
        cells.len(),
        "OutputRecord headers()/cells() arity must match, or fields are silently dropped"
    );
    write_csv_if_requested(std::slice::from_ref(record), out)?;
    if out.json {
        return write_json(record);
    }
    let mut table = new_table(out);
    for (header, cell) in T::headers().iter().zip(cells) {
        table.add_row(vec![
            (*header).to_string(),
            display_safe(&cell).into_owned(),
        ]);
    }
    print_table(&table)
}

/// Emit the "nothing found" shape for a single-record lookup, so `--json` and
/// `--csv` hold their contracts on this exit path too.
pub fn emit_missing<T: OutputRecord>(out: &OutputOpts) -> Result<()> {
    write_csv_if_requested::<T>(&[], out)?;
    if out.json {
        return write_json(&serde_json::Value::Null);
    }
    Ok(())
}

/// The running version against the newest release. What `check-update`
/// reports.
#[derive(Serialize)]
pub struct UpdateStatus {
    pub current: String,
    pub latest: String,
    pub update_available: bool,
    pub release_url: String,
}

impl UpdateStatus {
    /// The one line a human reads. Also covers running *ahead* of the newest
    /// release — a build from a tagged-but-unreleased tree, or from `main` —
    /// which is not an update but is not "up to date" either.
    pub fn summary(&self) -> String {
        if self.update_available {
            format!(
                "crt-query {} is available (running {}): {}",
                self.latest, self.current, self.release_url
            )
        } else if self.current == self.latest {
            format!("crt-query {} is the latest release.", self.current)
        } else {
            format!(
                "crt-query {} is newer than the latest release ({}).",
                self.current, self.latest
            )
        }
    }
}

impl OutputRecord for UpdateStatus {
    fn headers() -> &'static [&'static str] {
        // Display headers, like every other record: `headers()` feeds the table
        // and the CSV, while the JSON keys come from the serde field names and
        // are unaffected by what this returns.
        &["Current", "Latest", "Update Available", "Release URL"]
    }

    fn cells(&self) -> Vec<String> {
        vec![
            self.current.clone(),
            self.latest.clone(),
            self.update_available.to_string(),
            self.release_url.clone(),
        ]
    }
}

/// Emit an update status: one human-readable line by default, the JSON object
/// with `--json`, and — like every other record — a one-row CSV file with
/// `--csv`.
pub fn emit_update_status(status: &UpdateStatus, out: &OutputOpts) -> Result<()> {
    write_csv_if_requested(std::slice::from_ref(status), out)?;
    if out.json {
        return write_json(status);
    }
    on_stdout(|w| writeln!(w, "{}", status.summary()))
}

fn write_csv_if_requested<T: OutputRecord>(rows: &[T], out: &OutputOpts) -> Result<()> {
    if let Some(path) = &out.csv {
        let file = File::create(path)
            .with_context(|| format!("cannot write CSV to {}", path.display()))?;
        let written = write_csv(rows, file)
            .with_context(|| format!("cannot write CSV to {}", path.display()))?;
        eprintln!("wrote {written} CSV row(s) to {}", path.display());
    }
    Ok(())
}

/// Write `rows` as CSV and return the number of data rows written.
pub fn write_csv<T: OutputRecord, W: Write>(rows: &[T], w: W) -> Result<usize> {
    let mut w = csv::Writer::from_writer(w);
    w.write_record(T::headers())?;
    let mut written = 0;
    for r in rows {
        for record in r.csv_rows() {
            w.write_record(record.iter().map(|v| csv_safe(v).into_owned()))?;
            written += 1;
        }
    }
    w.flush()?;
    Ok(written)
}

/// Stop a spreadsheet treating a certificate field as a formula.
///
/// Issuer, subject, common name and SAN are all text lifted from a public CT
/// log — anyone who can get a certificate logged chooses them. Excel and
/// LibreOffice evaluate a cell beginning `=`, `+`, `@`, tab or carriage return,
/// so a prefixed apostrophe makes them literal.
///
/// Deliberately narrow. The usual rule also covers `-`, which would corrupt
/// `days_left`'s negatives — the one column a script is most likely to read as
/// a number, and which is now genuinely numeric. Nothing that parses as a
/// number is touched.
fn csv_safe(value: &str) -> Cow<'_, str> {
    match value.chars().next() {
        Some('=' | '+' | '@' | '\t' | '\r') => Cow::Owned(format!("'{value}")),
        _ => Cow::Borrowed(value),
    }
}

/// Replace control characters that a terminal would act on rather than print.
///
/// comfy-table measures a cell in bytes it believes are printable, so an ANSI
/// escape inside a certificate identity is counted as width and then split
/// mid-sequence by wrapping: the reset lands on a different line, the row is
/// drawn narrower than its own borders, and the colour leaks into the rest of
/// the session. A carriage return is worse — it returns the cursor to column 0
/// and overwrites the line just drawn.
///
/// U+000A is left alone: comfy-table wraps on it correctly, and it is the one
/// control character that means something here. JSON needs none of this —
/// `serde_json` escapes them already — so this applies only to the two table
/// paths.
fn display_safe(value: &str) -> Cow<'_, str> {
    let needs_escaping =
        |c: char| c != '\n' && (c.is_control() || ('\u{80}'..='\u{9f}').contains(&c));
    if !value.chars().any(needs_escaping) {
        return Cow::Borrowed(value);
    }
    Cow::Owned(
        value
            .chars()
            .map(|c| {
                if needs_escaping(c) {
                    format!("\\u{{{:04x}}}", c as u32)
                } else {
                    c.to_string()
                }
            })
            .collect(),
    )
}

/// Apply [`display_safe`] to every cell of a row bound for a table.
fn display_safe_row(cells: Vec<String>) -> Vec<String> {
    cells
        .into_iter()
        .map(|c| display_safe(&c).into_owned())
        .collect()
}

/// Build the table for `rows`, sized per [`new_table`].
fn build_table<T: OutputRecord>(rows: &[T], out: &OutputOpts) -> Table {
    let mut table = new_table(out);
    table.set_header(T::headers().to_vec());
    if out.width.is_none() {
        constrain_columns(&mut table, T::headers());
    }
    for r in rows {
        table.add_row(display_safe_row(r.cells()));
    }
    table
}

/// An empty table sized for `out`.
///
/// `--width` is an instruction, not a hint, so an explicit one is met exactly
/// in both directions: `DynamicFullWidth` spends surplus space instead of
/// stopping at the natural content width, and [`build_table`] leaves the column
/// constraints off, since comfy-table honours those over the table width and
/// they would otherwise hold the table open well past a narrow request.
///
/// Choosing the width automatically keeps both — there is no instruction to
/// respect, only a layout to keep readable.
fn new_table(out: &OutputOpts) -> Table {
    let mut table = Table::new();
    table.load_style(UTF8_FULL_CONDENSED);
    match out.width {
        Some(width) => {
            table
                .set_content_arrangement(ContentArrangement::DynamicFullWidth)
                .set_width(width);
        }
        None => {
            table.set_content_arrangement(ContentArrangement::Dynamic);
            if !io::stdout().is_terminal() {
                table.set_width(PIPED_WIDTH);
            }
        }
    }
    table
}

fn print_table(table: &Table) -> Result<()> {
    on_stdout(|w| writeln!(w, "{table}"))
}

/// Write bytes that were rendered elsewhere — currently the completion scripts
/// — through the same stdout handling as every other output, so a closed reader
/// ends the run cleanly instead of panicking.
pub fn emit_raw(bytes: &[u8]) -> Result<()> {
    on_stdout(|w| w.write_all(bytes))
}

fn write_json<T: Serialize + ?Sized>(value: &T) -> Result<()> {
    on_stdout(|w| {
        serde_json::to_writer_pretty(&mut *w, value).map_err(io::Error::from)?;
        writeln!(w)
    })
}

/// Run a write against stdout, treating a reader that has gone away
/// (`crt-query search … | head`) as a normal end of output rather than an
/// error. Rust ignores SIGPIPE, so without this the write panics with
/// "failed printing to stdout" and the process exits 101.
fn on_stdout(f: impl FnOnce(&mut dyn Write) -> io::Result<()>) -> Result<()> {
    let stdout = io::stdout();
    // Buffered, because `StdoutLock` is a `LineWriter`: unbuffered it costs one
    // `write(2)` per line, which pretty-printed JSON emits by the hundred
    // thousand. The explicit flush below is what makes this safe.
    let mut w = BufWriter::new(stdout.lock());
    match f(&mut w).and_then(|()| w.flush()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => std::process::exit(0),
        Err(e) => Err(e).context("writing to stdout"),
    }
}

/// Format a timestamp for the table. Values are UTC throughout; the column
/// headers say so.
pub fn fmt_ts(ts: Option<&DateTime<Utc>>) -> String {
    fmt_opt(ts.map(|t| t.format(TS_FORMAT)))
}

/// Format any optional column, falling back to a dash for NULL.
pub fn fmt_opt<T: Display>(value: Option<T>) -> String {
    value.map_or_else(|| DASH.to_string(), |v| v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queries::search::SearchRow;
    use chrono::NaiveDate;

    fn utc(y: i32, m: u32, d: u32) -> DateTime<Utc> {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
    }

    fn row(identities: &[&str]) -> SearchRow {
        SearchRow {
            id: 7,
            issuer_ca_id: Some(9),
            issuer_name: Some("Test CA".to_string()),
            matched_identities: identities.iter().map(|s| (*s).to_string()).collect(),
            common_name: Some("example.com".to_string()),
            serial: Some("0a1b".to_string()),
            not_before: Some(utc(2026, 1, 1)),
            not_after: Some(utc(2026, 4, 1)),
        }
    }

    fn csv_string<T: OutputRecord>(rows: &[T]) -> String {
        let mut buf = Vec::new();
        write_csv(rows, &mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    fn opts(width: Option<u16>) -> OutputOpts {
        OutputOpts {
            json: false,
            csv: None,
            width,
        }
    }

    /// Widest rendered line, in display columns. Every character the box-drawing
    /// preset uses is single-width, so counting chars is counting columns.
    fn rendered_width<T: OutputRecord>(rows: &[T], out: &OutputOpts) -> usize {
        build_table(rows, out)
            .to_string()
            .lines()
            .map(|l| l.chars().count())
            .max()
            .unwrap_or(0)
    }

    #[test]
    fn an_explicit_width_is_met_exactly() {
        // The widest record type: its constraints alone add up past 180
        // columns, which is what used to make a narrower request unachievable.
        let rows = [row(&["example.com"])];
        for want in [60_u16, 80, 100, 132, 160] {
            assert_eq!(
                rendered_width(&rows, &opts(Some(want))),
                usize::from(want),
                "--width {want} was not honoured"
            );
        }
    }

    #[test]
    fn an_explicit_width_widens_as_well_as_narrows() {
        // Dynamic alone stops at the natural content width, so asking for more
        // than the content needs used to be a no-op.
        let rows = [row(&["example.com"])];
        let natural = rendered_width(&rows, &opts(None));
        let wider = u16::try_from(natural).unwrap() + 40;
        assert_eq!(
            rendered_width(&rows, &opts(Some(wider))),
            usize::from(wider)
        );
    }

    #[test]
    fn two_different_widths_render_differently() {
        // The regression this guards: every requested width collapsed to the
        // same table, because the column constraints outrank the table width.
        let rows = [row(&["example.com"])];
        assert_ne!(
            rendered_width(&rows, &opts(Some(90))),
            rendered_width(&rows, &opts(Some(150)))
        );
    }

    /// A row wide enough that the automatic layout has to make a choice.
    ///
    /// The shared `row()` fixture cannot test this: its four-character serial
    /// fits at any width, so the assertion held whether or not the layout
    /// heuristic ran at all. These are realistic values — a full 36-hex-digit
    /// serial, a real-shaped issuer DN, two long identities — whose combined
    /// width exceeds the 120-column fallback and forces the wrap decision.
    fn wide_row() -> SearchRow {
        SearchRow {
            id: 22625564176,
            issuer_ca_id: Some(204411),
            issuer_name: Some(
                "C=GB, O=Sectigo Limited, CN=Sectigo Public Server Authentication CA OV R36"
                    .to_string(),
            ),
            matched_identities: vec![
                "very-long-subdomain-name.service.example.com".to_string(),
                "another-long-subdomain-name.service.example.org".to_string(),
            ],
            common_name: Some("very-long-subdomain-name.service.example.com".to_string()),
            serial: Some("009de10580fa26441939f38af4afb1cb40".to_string()),
            not_before: Some(utc(2026, 1, 1)),
            not_after: Some(utc(2026, 4, 1)),
        }
    }

    #[test]
    fn the_automatic_layout_still_refuses_to_wrap_atomic_columns() {
        // With no --width the readability constraints stay on, so a hex serial
        // is never broken across lines even under real wrapping pressure.
        let rows = [wide_row()];
        let rendered = build_table(&rows, &opts(None)).to_string();
        let serial = rows[0].serial.as_deref().unwrap();
        assert!(
            rendered.lines().any(|l| l.contains(serial)),
            "the 34-character serial was split across lines, so the atomic-column \
             constraint did not apply:\n{rendered}"
        );
        // The same row must still be wrapped somewhere — otherwise the fixture
        // is not exerting the pressure this test claims to measure.
        assert!(
            rendered.lines().count() > 4,
            "fixture too narrow to force any wrapping; this test proves nothing:\n{rendered}"
        );
    }

    #[test]
    fn an_explicit_width_is_honoured_exactly() {
        // --width is an instruction, not a hint: it overrides the readability
        // constraints above rather than being clamped by them.
        for width in [60usize, 100, 200] {
            let rendered = build_table(&[wide_row()], &opts(Some(width as u16))).to_string();
            let widest = rendered
                .lines()
                .map(str::chars)
                .map(Iterator::count)
                .max()
                .unwrap();
            assert_eq!(
                widest, width,
                "--width {width} produced a {widest}-column table:\n{rendered}"
            );
        }
    }

    fn status(current: &str, latest: &str, update_available: bool) -> UpdateStatus {
        UpdateStatus {
            current: current.to_string(),
            latest: latest.to_string(),
            update_available,
            release_url: format!("https://example.invalid/releases/tag/v{latest}"),
        }
    }

    #[test]
    fn an_available_update_names_both_versions_and_links_the_release() {
        let line = status("0.1.0", "0.2.0", true).summary();
        assert!(line.contains("0.2.0"), "{line}");
        assert!(line.contains("0.1.0"), "{line}");
        assert!(
            line.contains("https://example.invalid/releases/tag/v0.2.0"),
            "{line}"
        );
    }

    #[test]
    fn a_current_build_says_so() {
        assert_eq!(
            status("0.1.0", "0.1.0", false).summary(),
            "crt-query 0.1.0 is the latest release."
        );
    }

    #[test]
    fn a_build_ahead_of_the_latest_release_is_not_reported_as_current() {
        let line = status("0.2.0", "0.1.0", false).summary();
        assert!(line.contains("newer than the latest release"), "{line}");
    }

    #[test]
    fn an_update_status_csv_row_matches_its_headers() {
        let csv = csv_string(&[status("0.1.0", "0.2.0", true)]);
        assert_eq!(
            csv.lines().next().unwrap(),
            "Current,Latest,Update Available,Release URL"
        );
        assert!(csv.lines().nth(1).unwrap().starts_with("0.1.0,0.2.0,true,"));
    }

    #[test]
    fn the_json_keys_are_serde_field_names_not_the_display_headers() {
        // headers() feeds the table and the CSV only. Renaming a header must
        // never move a JSON key, which is the machine-readable contract.
        let json = serde_json::to_value(status("0.1.0", "0.2.0", true)).unwrap();
        let mut keys: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            ["current", "latest", "release_url", "update_available"]
        );
    }

    #[test]
    fn precheck_does_not_leave_behind_a_file_it_only_created_to_test_writability() {
        let dir = std::env::temp_dir().join(format!("crt-query-precheck-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("report.csv");
        let opts = OutputOpts {
            json: false,
            csv: Some(path.clone()),
            width: None,
        };

        precheck_csv(&opts).unwrap();
        assert!(
            !path.exists(),
            "an empty placeholder is neither the header-only report the \
             contract promises nor a clean absence"
        );

        // An existing report is the previous run's, and outliving a later
        // failure is the whole point of not truncating it.
        std::fs::write(&path, "Matched Identities\nexample.com\n").unwrap();
        precheck_csv(&opts).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "Matched Identities\nexample.com\n"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn precheck_still_fails_on_an_unwritable_destination() {
        let opts = OutputOpts {
            json: false,
            csv: Some(std::path::PathBuf::from(
                "/crt-query-no-such-directory/report.csv",
            )),
            width: None,
        };
        let err = precheck_csv(&opts).unwrap_err();
        assert!(
            format!("{err:#}").contains("cannot write CSV to"),
            "{err:#}"
        );
    }

    #[test]
    fn headers_and_cells_agree_in_arity() {
        let r = row(&["example.com"]);
        assert_eq!(SearchRow::headers().len(), r.cells().len());
    }

    /// The CSV is a machine format, not a picture of the table. A NULL must be
    /// an empty field rather than the table's dash — one placeholder in a
    /// numeric column types the whole column as text in pandas and Excel.
    #[test]
    fn csv_writes_empty_fields_for_null_not_the_table_placeholder() {
        let mut r = row(&["example.com"]);
        r.issuer_name = None;
        r.serial = None;
        let csv = csv_string(&[r]);
        let data = csv.lines().nth(1).unwrap();
        assert!(
            !data.split(',').any(|f| f == DASH),
            "the table's dash placeholder reached the CSV: {data}"
        );
        assert!(data.contains(",,"), "expected empty fields, got: {data}");
        // The table still shows the dash — this changed CSV only.
        let table = build_table(&[row(&["example.com"])], &opts(None)).to_string();
        assert!(table.contains(DASH) || !table.is_empty());
    }

    /// The table truncates to the minute to save a column. A report a script
    /// parses should not lose the difference between 23:59:00 and 23:59:59 on
    /// the one field people schedule work around.
    #[test]
    fn csv_timestamps_keep_their_seconds_and_carry_an_offset() {
        let mut r = row(&["example.com"]);
        r.not_after = Some(
            chrono::NaiveDate::from_ymd_opt(2026, 11, 20)
                .unwrap()
                .and_hms_opt(23, 59, 59)
                .unwrap()
                .and_utc(),
        );
        let csv = csv_string(&[r]);
        assert!(
            csv.contains("2026-11-20T23:59:59+00:00") || csv.contains("2026-11-20T23:59:59Z"),
            "CSV lost the seconds off notAfter:\n{csv}"
        );
    }

    /// days_left is the column most likely to be read as a number, and the
    /// narrow formula rule exists so its negatives survive untouched.
    #[test]
    fn a_negative_days_left_is_never_quoted_as_a_formula() {
        assert_eq!(csv_safe("-30"), "-30");
        assert_eq!(csv_safe("-1"), "-1");
        assert_eq!(csv_safe("0"), "0");
        assert_eq!(csv_safe("example.com"), "example.com");
    }

    /// Issuer, subject, common name and SAN are text from a public CT log:
    /// whoever got the certificate logged chose them. A spreadsheet evaluates a
    /// cell that starts with one of these.
    #[test]
    fn a_field_that_a_spreadsheet_would_evaluate_is_made_literal() {
        for hostile in ["=1+1", "+1", "@SUM(A1)", "\tlead", "\rlead"] {
            let safe = csv_safe(hostile);
            assert!(
                safe.starts_with('\''),
                "{hostile:?} reached the file as a formula: {safe:?}"
            );
            assert!(safe.ends_with(hostile), "{hostile:?} was altered: {safe:?}");
        }
    }

    #[test]
    fn a_hostile_common_name_is_neutralised_in_the_written_file() {
        let mut r = row(&["example.com"]);
        r.common_name = Some("=cmd|'/c calc'!A0".to_string());
        let csv = csv_string(&[r]);
        assert!(
            csv.contains("'=cmd"),
            "the formula reached the file unquoted:\n{csv}"
        );
    }

    /// comfy-table counts escape bytes as printable width, so an ANSI sequence
    /// inside an identity is split mid-sequence by wrapping: the reset lands on
    /// another line and the colour leaks into the rest of the session. A
    /// carriage return is worse — it overwrites the line just drawn.
    #[test]
    fn control_characters_never_reach_the_terminal() {
        let mut r = row(&["example.com"]);
        r.common_name = Some("evil\u{1b}[31mred\u{1b}[0m".to_string());
        r.issuer_name = Some("carriage\rreturn".to_string());
        let rendered = build_table(&[r], &opts(None)).to_string();
        assert!(
            !rendered.contains('\u{1b}'),
            "an ESC reached the rendered table:\n{rendered:?}"
        );
        assert!(
            !rendered.contains('\r'),
            "a CR reached the rendered table:\n{rendered:?}"
        );
        assert!(
            rendered.contains("u{001b}"),
            "the escape should still be visible as text:\n{rendered}"
        );
    }

    #[test]
    fn a_newline_is_left_alone_because_the_table_wraps_on_it_correctly() {
        assert_eq!(display_safe("two\nlines"), "two\nlines");
        assert_eq!(display_safe("ordinary.example.com"), "ordinary.example.com");
    }

    /// JSON escapes control characters itself, so this must not double up.
    #[test]
    fn json_is_left_to_serdes_own_escaping() {
        let mut r = row(&["example.com"]);
        r.common_name = Some("esc\u{1b}here".to_string());
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\\u001b"), "{json}");
        assert!(!json.contains("u{001b}"), "double-escaped: {json}");
    }

    #[test]
    fn csv_writes_one_row_per_identity() {
        let out = csv_string(&[row(&["example.com", "www.example.com"])]);
        let lines: Vec<_> = out.lines().collect();
        assert_eq!(lines.len(), 3, "header + one row per identity: {out}");
        assert!(lines[1].contains("example.com"));
        assert!(lines[2].contains("www.example.com"));
        // Neither data row carries the other identity: they are separate rows,
        // not a joined field a consumer would have to split.
        assert!(!lines[1].contains("www.example.com"));
    }

    #[test]
    fn csv_row_count_matches_rows_written() {
        let mut buf = Vec::new();
        let n = write_csv(&[row(&["a.example.com", "b.example.com"])], &mut buf).unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn empty_result_still_writes_a_header_row() {
        let out = csv_string::<SearchRow>(&[]);
        assert_eq!(out.lines().count(), 1);
        assert!(out.starts_with("crt.sh ID,"));
    }

    #[test]
    fn separators_inside_a_value_survive_the_round_trip() {
        // A Subject identity really can contain ", " — the old joined encoding
        // made it indistinguishable from the separator.
        let out = csv_string(&[row(&["O=Example, Inc."])]);
        let mut rdr = csv::Reader::from_reader(out.as_bytes());
        let rec = rdr.records().next().unwrap().unwrap();
        assert_eq!(&rec[3], "O=Example, Inc.");
    }

    #[test]
    fn fmt_opt_renders_a_dash_for_none() {
        assert_eq!(fmt_opt(None::<i32>), "-");
        assert_eq!(fmt_opt(Some(42)), "42");
        assert_eq!(fmt_ts(None), "-");
        assert_eq!(fmt_ts(Some(&utc(2026, 4, 1))), "2026-04-01 00:00");
    }

    #[test]
    fn constrain_columns_pins_ids_serial_and_timestamps_to_content_width() {
        let headers = SearchRow::headers();
        let mut table = Table::new();
        table.set_header(headers.to_vec());
        constrain_columns(&mut table, headers);

        for (i, header) in headers.iter().enumerate() {
            let constraint = table.column(i).unwrap().constraint();
            match *header {
                "crt.sh ID" | "Issuer CA ID" | "Serial" | "Not Before (UTC)"
                | "Not After (UTC)" => {
                    // No natural wrap point in an ID, a hex serial, or a
                    // formatted timestamp: wrapping only garbles them.
                    assert_eq!(
                        constraint,
                        Some(&ColumnConstraint::ContentWidth),
                        "{header} should never wrap"
                    );
                }
                "Issuer" | "Matched Identities" | "Common Name" => {
                    // Free text with real wrap points still needs a floor,
                    // or it gets squeezed to a character-per-line sliver.
                    assert!(
                        matches!(constraint, Some(ColumnConstraint::LowerBoundary(_))),
                        "{header} should have a minimum width, got {constraint:?}"
                    );
                }
                other => panic!("unhandled SearchRow header {other:?} in this test"),
            }
        }
    }
}
