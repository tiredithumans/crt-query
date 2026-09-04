//! Neutralising certificate text before it reaches a terminal or a spreadsheet.
//!
//! Every value these functions see — issuer, subject, common name, SAN,
//! matched identity — was chosen by whoever got the certificate logged, so
//! the threat model is "attacker-controlled text in a trusted-looking report".
//! Two consumers, two rules: [`display_safe`] for what comfy-table renders in a
//! terminal, [`csv_safe`] for what a spreadsheet will open. JSON is deliberately
//! left to serde's own escaping; see [`display_safe`].

use std::borrow::Cow;

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
/// set: *a `-`-led field that parses as a finite number is left alone*. Test
/// that directly: `-30` is left alone while `-2+3+cmd|' /C calc'!A0` is
/// neutralised.
///
/// Finite, because `f64::from_str` is wider than "a number this tool writes":
/// it accepts `inf`, `infinity` and `nan` in any case, and rounds an
/// overflowing exponent such as `1e999` to infinity. None of those is a value
/// any column here emits, and a spreadsheet evaluates `-inf` as a formula —
/// `#NAME?` in the cell rather than the text the log holds — so they stay
/// quoted like any other `-`-led text.
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
pub(super) fn csv_safe(value: &str) -> Cow<'_, str> {
    let value = bidi_safe(value);
    let leads_a_formula = match value.chars().next() {
        Some('-') => !value.parse::<f64>().is_ok_and(f64::is_finite),
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
pub(super) fn display_safe_row(cells: Vec<String>) -> Vec<String> {
    cells
        .into_iter()
        .map(|c| display_safe(&c).into_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// `f64::from_str` accepts more than the numbers this tool writes: `inf`,
    /// `infinity` and `nan` in any case, and an overflowing exponent rounds to
    /// infinity. A spreadsheet evaluates `-inf` as a formula (`#NAME?`), so
    /// the exemption is for finite numbers only — the invariant as documented,
    /// which the bare `parse().is_err()` did not actually implement.
    #[test]
    fn a_hyphen_led_non_finite_float_is_quoted() {
        for hostile in ["-inf", "-Inf", "-Infinity", "-nan", "-NaN", "-1e999"] {
            assert!(
                hostile.parse::<f64>().is_ok(),
                "{hostile:?} no longer parses as f64; this case is moot"
            );
            let safe = csv_safe(hostile);
            assert!(
                safe.starts_with('\''),
                "{hostile:?} reached the file as a formula: {safe:?}"
            );
        }
        // Exponents that stay finite are still numbers.
        assert_eq!(csv_safe("-1e5"), "-1e5");
        assert_eq!(csv_safe("-1.5e-3"), "-1.5e-3");
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

    #[test]
    fn a_newline_is_left_alone_because_the_table_wraps_on_it_correctly() {
        assert_eq!(display_safe("two\nlines"), "two\nlines");
        assert_eq!(display_safe("ordinary.example.com"), "ordinary.example.com");
    }
}
