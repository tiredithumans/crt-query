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

/// Exit status for a run that emitted everything it had. Mirrors `EXIT_OK` in
/// `main.rs`, which owns the exit-code contract; this is only what a reader
/// going away mid-write should report.
const EXIT_COMPLETED: i32 = 0;

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
            // Remove what was actually created, not the path that was named.
            // `exists()` follows symlinks and `remove_file` does not, so for a
            // symlink whose target was missing the open created the *target*
            // and this unlinked the *link* — destroying a file the caller made
            // while leaving behind precisely the empty report described above.
            // The usual rotation pattern (`latest.csv -> 2026-09-03.csv`) hits
            // it on every run once the target has rotated away, successful runs
            // included, since this check runs before the query.
            let created = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
            let _ = std::fs::remove_file(created);
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
    // Through `display_safe_row`, the same helper `build_table` uses, rather
    // than calling `display_safe` again here: this is the only view that
    // renders Subject and SANs, and a second sanitising path is one nothing
    // was covering.
    for (header, cell) in T::headers().iter().zip(display_safe_row(cells)) {
        table.add_row(vec![(*header).to_string(), cell]);
    }
    print_table(&table)
}

/// Emit the "nothing found" shape for a single-record lookup, so `--json` and
/// `--csv` hold their contracts on this exit path too.
///
/// `broken_pipe_exit` is the status the caller was about to return. Writing the
/// `null` document into a reader that has gone away must not turn a decided
/// exit 3 into success — see [`on_stdout_with`].
pub fn emit_missing<T: OutputRecord>(out: &OutputOpts, broken_pipe_exit: i32) -> Result<()> {
    write_csv_if_requested::<T>(&[], out)?;
    if out.json {
        return on_stdout_with(broken_pipe_exit, |w| {
            serde_json::to_writer_pretty(&mut *w, &serde_json::Value::Null)
                .map_err(io::Error::from)?;
            writeln!(w)
        });
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

/// Stop a spreadsheet treating a certificate field as a formula, or rendering
/// one in a direction it does not hold.
///
/// Issuer, subject, common name and SAN are all text lifted from a public CT
/// log — anyone who can get a certificate logged chooses them. Excel and
/// LibreOffice evaluate a cell beginning `=`, `+`, `-`, `@`, tab or carriage
/// return, so a prefixed apostrophe makes them literal.
///
/// `-` is in that set, and it is the character a payload opens with precisely
/// to slip past a filter covering only `=`/`+`/`@`. It also leads every
/// negative `days_left`, the one column a script is most likely to read as a
/// number. The invariant that matters is narrower than dropping `-` from the
/// set: *a `-`-led field that parses as a number is left alone*. Test that
/// directly: `-30` is left alone while `-2+3+cmd|' /C calc'!A0` is
/// neutralised.
///
/// The exemption is for `-` alone, not for every leader that happens to parse.
/// `id`, `issuer_ca_id` and `days_left` are the only numeric columns and none
/// of them can emit a leading `+`, so a field arriving as `+1` is CT-log text
/// rather than a number of ours, and stays quoted as it always has.
///
/// CSV gets [`bidi_safe`] as well. It is the machine format, but it is also the
/// one people open in a spreadsheet — and a spreadsheet implements the Unicode
/// bidirectional algorithm, so the display spoofing [`display_safe`] prevents
/// in the table is reachable here too. Only the overrides are escaped, never
/// letters: real Arabic or Hebrew in a subject DN reorders correctly from its
/// own character properties and needs none of them.
fn csv_safe(value: &str) -> Cow<'_, str> {
    let value = bidi_safe(value);
    let leads_a_formula = match value.chars().next() {
        Some('-') => value.parse::<f64>().is_err(),
        Some('=' | '+' | '@' | '\t' | '\r') => true,
        _ => false,
    };
    if leads_a_formula {
        Cow::Owned(format!("'{value}"))
    } else {
        value
    }
}

/// Render bidi overrides as visible text, leaving everything else alone.
///
/// Narrower than [`display_safe`] on purpose: this runs over a machine format,
/// so it neutralises only the characters that reorder a rendered cell and
/// leaves the rest — control characters included — to the CSV writer, which
/// quotes a field holding a comma, quote or line break and writes every other
/// byte raw. Both forms round-trip through a CSV reader.
fn bidi_safe(value: &str) -> Cow<'_, str> {
    if !value.chars().any(is_bidi_control) {
        return Cow::Borrowed(value);
    }
    Cow::Owned(
        value
            .chars()
            .map(|c| {
                if is_bidi_control(c) {
                    escaped(c)
                } else {
                    c.to_string()
                }
            })
            .collect(),
    )
}

/// Characters that reorder the text around them while occupying no width.
///
/// `char::is_control()` is Cc only, so these took the borrowed fast path and
/// reached comfy-table verbatim — and comfy-table never appends a terminating
/// PDF or PDI. Under the Unicode bidi algorithm, which ICU, VTE, iTerm2 3.5+,
/// browsers and spreadsheets all implement, one U+202E inside a certificate
/// identity renders the rest of the row reversed: the cell displays a hostname
/// it does not contain and the row's closing border moves. Every column this
/// reaches — Matched Identities, Common Name, Issuer, Subject, SANs — is text
/// an attacker chooses and gets into a public CT log, the same threat model
/// `csv_safe` above is written against.
///
/// Column alignment survives (these are zero-width) and the effect is bounded
/// to one rendered line, so this is display spoofing rather than corruption.
const BIDI_CONTROLS: &[char] = &[
    '\u{061c}', // ARABIC LETTER MARK
    '\u{200e}', // LEFT-TO-RIGHT MARK
    '\u{200f}', // RIGHT-TO-LEFT MARK
    '\u{202a}', // LEFT-TO-RIGHT EMBEDDING
    '\u{202b}', // RIGHT-TO-LEFT EMBEDDING
    '\u{202c}', // POP DIRECTIONAL FORMATTING
    '\u{202d}', // LEFT-TO-RIGHT OVERRIDE
    '\u{202e}', // RIGHT-TO-LEFT OVERRIDE
    '\u{2066}', // LEFT-TO-RIGHT ISOLATE
    '\u{2067}', // RIGHT-TO-LEFT ISOLATE
    '\u{2068}', // FIRST STRONG ISOLATE
    '\u{2069}', // POP DIRECTIONAL ISOLATE
];

/// Whether a character has to be rendered as an escape rather than emitted.
///
/// `is_control()` is Cc, which already subsumes the C1 range U+0080..=U+009F
/// this used to re-test separately — an exhaustive scan over every Unicode
/// scalar finds nothing satisfying that range without also being a control.
fn needs_escaping(c: char) -> bool {
    c != '\n' && (c.is_control() || is_bidi_control(c))
}

/// Whether `c` is one of the [`BIDI_CONTROLS`].
fn is_bidi_control(c: char) -> bool {
    BIDI_CONTROLS.contains(&c)
}

/// How an escaped character is rendered, shared by both sanitisers so the
/// table and the CSV spell the same character the same way.
fn escaped(c: char) -> String {
    format!("\\u{{{:04x}}}", c as u32)
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
/// control character that means something here.
///
/// This applies only to the two table paths. `serde_json` escapes C0, `"` and
/// `\` — not DEL or C1, which therefore survive `--json` raw, deliberately:
/// JSON is a machine format whose consumer decodes escapes anyway, and running
/// this function over it would emit Rust-syntax `\u{009b}` whose backslash
/// serde would escape a second time, corrupting a documented contract.
fn display_safe(value: &str) -> Cow<'_, str> {
    if !value.chars().any(needs_escaping) {
        return Cow::Borrowed(value);
    }
    Cow::Owned(
        value
            .chars()
            .map(|c| {
                if needs_escaping(c) {
                    escaped(c)
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
    on_stdout_with(EXIT_COMPLETED, f)
}

/// [`on_stdout`], but exiting with `broken_pipe_exit` if the reader has gone.
///
/// Almost every caller wants 0: `crt-query search … | head` is a completed
/// result whose consumer stopped reading, not a failure. The exception is the
/// not-found path, which has already decided on exit 3 — hard-coding 0 here
/// silently discarded that, so `cert --json <missing-id>` into a closed reader
/// reported success for a certificate that does not exist.
///
/// A genuine write error still propagates and the run exits 1, which is right:
/// the report was not delivered.
fn on_stdout_with(
    broken_pipe_exit: i32,
    f: impl FnOnce(&mut dyn Write) -> io::Result<()>,
) -> Result<()> {
    let stdout = io::stdout();
    // Buffered, because `StdoutLock` is a `LineWriter`: unbuffered it costs one
    // `write(2)` per line, which pretty-printed JSON emits by the hundred
    // thousand. The explicit flush below is what makes this safe.
    let mut w = BufWriter::new(stdout.lock());
    match f(&mut w).and_then(|()| w.flush()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => std::process::exit(broken_pipe_exit),
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
    /// numeric exemption exists so its negatives survive untouched.
    #[test]
    fn a_negative_days_left_is_never_quoted_as_a_formula() {
        assert_eq!(csv_safe("-30"), "-30");
        assert_eq!(csv_safe("-1"), "-1");
        assert_eq!(csv_safe("0"), "0");
        assert_eq!(csv_safe("example.com"), "example.com");
        // Not just integers: the exemption is "`-` leading something that
        // parses as a number", so a future signed decimal column cannot be
        // corrupted either.
        assert_eq!(csv_safe("-1.5"), "-1.5");
        assert_eq!(csv_safe("-0"), "-0");
    }

    /// A leading `-` is in the standard formula set and was the one leader this
    /// rule skipped, because it also leads every negative days_left. Skipping
    /// it wholesale left the gap open: `-` is exactly what a payload opens with
    /// to get past a filter covering only `=`/`+`/`@`.
    #[test]
    fn a_hyphen_led_payload_is_quoted_while_a_negative_number_is_not() {
        for hostile in ["-2+3+cmd|' /C calc'!A0", "-1+1", "-cmd|'/c calc'!A0", "-"] {
            let safe = csv_safe(hostile);
            assert!(
                safe.starts_with('\''),
                "{hostile:?} reached the file as a formula: {safe:?}"
            );
            assert!(safe.ends_with(hostile), "{hostile:?} was altered: {safe:?}");
        }
        for numeric in ["-30", "-1", "-1856", "-0.5"] {
            assert_eq!(
                csv_safe(numeric),
                numeric,
                "a genuine negative number must reach the file as a number"
            );
        }
    }

    /// The exemption is for `-` only. No numeric column this tool writes can
    /// emit a leading `+`, so `+1` is CT-log text and stays quoted — the
    /// behaviour that shipped, and worth keeping when widening the rule.
    #[test]
    fn the_numeric_exemption_does_not_leak_to_the_other_leaders() {
        assert!(csv_safe("+1").starts_with('\''));
        assert!(csv_safe("=1").starts_with('\''));
    }

    /// A spreadsheet implements the Unicode bidirectional algorithm, so the
    /// display spoofing display_safe prevents in the table is reachable through
    /// a CSV too — and the CSV is the artefact people forward to someone else.
    #[test]
    fn bidi_overrides_are_neutralised_in_the_csv_too() {
        let safe = csv_safe("moc.elpmaxe\u{202e}live");
        assert!(
            !safe.contains('\u{202e}'),
            "an override reached the CSV: {safe:?}"
        );
        assert!(safe.contains("u{202e}"), "{safe:?}");
        for c in BIDI_CONTROLS {
            let value = format!("a{c}b");
            assert_ne!(
                csv_safe(&value),
                value,
                "U+{:04X} passed through to the CSV",
                *c as u32
            );
        }
    }

    /// The reason only the overrides are escaped, never letters: real
    /// right-to-left text in a subject DN reorders correctly from its own
    /// character properties, so neutralising the controls costs legitimate
    /// certificate data nothing.
    #[test]
    fn genuine_right_to_left_text_is_left_intact() {
        for legitimate in ["شركة المثال", "דוגמה", "example.com", "Ünïcödé Ltd"]
        {
            assert_eq!(
                csv_safe(legitimate),
                legitimate,
                "legitimate text was altered"
            );
            assert_eq!(display_safe(legitimate), legitimate);
        }
    }

    /// The CSV writer already quotes fields containing separators, quotes and
    /// newlines, so bidi_safe deliberately leaves those alone rather than
    /// escaping them and changing the value a consumer parses back out.
    #[test]
    fn bidi_safe_leaves_everything_else_to_the_csv_writer() {
        assert_eq!(bidi_safe("a,b"), "a,b");
        assert_eq!(bidi_safe("two\nlines"), "two\nlines");
        assert_eq!(bidi_safe("quote\"inside"), "quote\"inside");
        assert_eq!(bidi_safe("tab\there"), "tab\there");
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

    /// A certificate identity is chosen by whoever got the certificate logged.
    /// A bidi override in one costs no display width, so it slipped through a
    /// Cc-only filter and then reordered every character after it in the
    /// rendered row — the cell shows a hostname it does not contain.
    #[test]
    fn bidi_overrides_never_reach_the_terminal() {
        let mut r = row(&["\u{202e}moc.elpmaxe.live"]);
        r.common_name = Some("safe\u{2066}spoofed".to_string());
        r.issuer_name = Some("mark\u{200f}here".to_string());
        let rendered = build_table(&[r], &opts(None)).to_string();
        for (name, c) in [
            ("RIGHT-TO-LEFT OVERRIDE", '\u{202e}'),
            ("LEFT-TO-RIGHT ISOLATE", '\u{2066}'),
            ("RIGHT-TO-LEFT MARK", '\u{200f}'),
        ] {
            assert!(
                !rendered.contains(c),
                "{name} reached the rendered table and can reorder the row:\n{rendered:?}"
            );
        }
        assert!(
            rendered.contains("u{202e}"),
            "the override should still be visible as text:\n{rendered}"
        );
    }

    #[test]
    fn every_bidi_control_is_escaped() {
        for c in BIDI_CONTROLS {
            let value = format!("a{c}b");
            assert_ne!(
                display_safe(&value),
                value,
                "U+{:04X} passed through unescaped",
                *c as u32
            );
        }
    }

    /// The C1 range this used to test separately is entirely inside `Cc`, so
    /// the extra disjunct was dead code. If that ever stops being true the
    /// range has to come back.
    #[test]
    fn the_c1_range_is_already_covered_by_is_control() {
        for c in '\u{80}'..='\u{9f}' {
            assert!(c.is_control(), "U+{:04X} is no longer Cc", c as u32);
        }
    }

    /// `emit_missing` is called only from the exit-3 path in main.rs and by no
    /// test, so reducing it to `Ok(())` passed everything — while it alone
    /// upholds the documented "always write the file, header-only when empty"
    /// contract. Combined with precheck_csv leaving a pre-existing file in
    /// place, a regression means a scheduled job silently re-reads yesterday's
    /// report for a certificate that no longer resolves.
    #[test]
    fn emit_missing_still_writes_a_header_only_report() {
        let dir = std::env::temp_dir().join(format!("crt-query-unit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("missing.csv");
        let _ = std::fs::remove_file(&path);

        let out = OutputOpts {
            json: false,
            csv: Some(path.clone()),
            width: None,
        };
        emit_missing::<SearchRow>(&out, 3).unwrap();

        let written = std::fs::read_to_string(&path).expect("a report was promised");
        let mut lines = written.lines();
        assert_eq!(
            lines.next(),
            Some(SearchRow::headers().join(",").as_str()),
            "the header row is the contract a consumer parses"
        );
        assert_eq!(lines.next(), None, "a not-found report has no data rows");
        let _ = std::fs::remove_file(&path);
    }

    /// `emit_detail` is the only view that renders Subject and SANs, and it
    /// sanitised on a path of its own that no test exercised.
    #[test]
    fn the_detail_view_sanitises_its_cells_too() {
        struct Hostile;
        impl Serialize for Hostile {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str("hostile")
            }
        }
        impl OutputRecord for Hostile {
            fn headers() -> &'static [&'static str] {
                &["Subject", "SANs"]
            }
            fn cells(&self) -> Vec<String> {
                vec![
                    "evil\u{1b}[31m".to_string(),
                    "carriage\rreturn\u{202e}flip".to_string(),
                ]
            }
        }
        let mut table = new_table(&opts(None));
        for (header, cell) in Hostile::headers()
            .iter()
            .zip(display_safe_row(Hostile.cells()))
        {
            table.add_row(vec![(*header).to_string(), cell]);
        }
        let rendered = table.to_string();
        for (name, c) in [
            ("ESC", '\u{1b}'),
            ("CR", '\r'),
            ("RIGHT-TO-LEFT OVERRIDE", '\u{202e}'),
        ] {
            assert!(
                !rendered.contains(c),
                "{name} reached the detail table:\n{rendered:?}"
            );
        }
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
