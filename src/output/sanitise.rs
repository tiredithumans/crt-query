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
/// Issuer, subject, common name and SAN are text lifted from a public CT
/// log — anyone who can get a certificate logged chooses them. Excel and
/// LibreOffice evaluate a cell beginning `=`, `+`, `-`, `@`, tab or CR, so a
/// prefixed apostrophe makes them literal.
///
/// `-` is in that set and is what a payload opens with to slip past a
/// `=`/`+`/`@` filter — but it also leads every negative `days_left`. The
/// invariant is narrower than dropping `-`: *a `-`-led field that parses as a
/// finite number is left alone* — `-30` passes, `-2+3+cmd|' /C calc'!A0` is
/// neutralised. Finite, because `f64::from_str` also accepts `inf`/`nan` and
/// rounds `1e999` to infinity, none of which any column emits, and a
/// spreadsheet evaluates `-inf` as a formula (`#NAME?`).
///
/// The exemption is for `-` alone: the numeric columns here can never emit a
/// leading `+`, so `+1` is CT-log text and stays quoted.
///
/// CSV gets [`format_control_safe`] as well — it is the artefact people
/// forward and open in a spreadsheet, which implements the Unicode
/// bidirectional algorithm. Only format controls are escaped, never letters:
/// real Arabic or Hebrew reorders correctly from its own character
/// properties.
pub(super) fn csv_safe(value: &str) -> Cow<'_, str> {
    let value = format_control_safe(value);
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

/// Render format controls — bidi overrides and zero-width characters — as
/// visible text, leaving everything else alone.
///
/// Narrower than [`display_safe`] on purpose: this runs over a machine format,
/// so it neutralises only the characters that make a rendered cell show text
/// it does not hold, and leaves the rest — control characters included — to
/// the CSV writer, which quotes a field holding a comma, quote or line break
/// and writes every other byte raw. Both forms round-trip through a CSV
/// reader.
fn format_control_safe(value: &str) -> Cow<'_, str> {
    if !value.chars().any(is_format_control) {
        return Cow::Borrowed(value);
    }
    Cow::Owned(
        value
            .chars()
            .map(|c| {
                if is_format_control(c) {
                    escaped(c)
                } else {
                    c.to_string()
                }
            })
            .collect(),
    )
}

/// Every character that changes how the text around it is shown while showing
/// nothing itself: Unicode general category Cf (format), plus the line and
/// paragraph separators U+2028 and U+2029 (Zl, Zp), which a terminal or
/// spreadsheet may break a line on without comfy-table knowing.
///
/// Ranges rather than a list of named characters, because the lists this
/// replaced were always one character short. They began with the bidi
/// overrides — one U+202E renders the rest of a row reversed, so the cell
/// shows a hostname it does not contain — then gained the zero-width spaces
/// after `gi\u{200b}thub.com` passed for `github.com`. A review then found
/// `exa\u{00ad}mple.com` (soft hyphen), U+2061..=U+2064 and the U+E0000 tag
/// block doing the same thing, all Cf and none listed. The category is the
/// rule; `BIDI_CONTROLS` and `ZERO_WIDTH_FORMATS` (test-only, so not linked)
/// stay as the tests' named cases inside it.
///
/// Generated from Python's `unicodedata` (Unicode 15.1). `char::is_control()`
/// is Cc only and std exposes no general category, and a Unicode crate would
/// be a new dependency for one table. A later Unicode release that adds a Cf
/// character leaves it unescaped until this table is regenerated.
const FORMAT_RANGES: &[(char, char)] = &[
    ('\u{00ad}', '\u{00ad}'),   // SOFT HYPHEN
    ('\u{0600}', '\u{0605}'),   // ARABIC NUMBER SIGN ..
    ('\u{061c}', '\u{061c}'),   // ARABIC LETTER MARK
    ('\u{06dd}', '\u{06dd}'),   // ARABIC END OF AYAH
    ('\u{070f}', '\u{070f}'),   // SYRIAC ABBREVIATION MARK
    ('\u{0890}', '\u{0891}'),   // ARABIC POUND/PIASTRE MARK ABOVE
    ('\u{08e2}', '\u{08e2}'),   // ARABIC DISPUTED END OF AYAH
    ('\u{180e}', '\u{180e}'),   // MONGOLIAN VOWEL SEPARATOR
    ('\u{200b}', '\u{200f}'),   // ZERO WIDTH SPACE .. RIGHT-TO-LEFT MARK
    ('\u{2028}', '\u{202e}'),   // LINE SEPARATOR .. RIGHT-TO-LEFT OVERRIDE
    ('\u{2060}', '\u{2064}'),   // WORD JOINER .. INVISIBLE PLUS
    ('\u{2066}', '\u{206f}'),   // LEFT-TO-RIGHT ISOLATE .. NOMINAL DIGIT SHAPES
    ('\u{feff}', '\u{feff}'),   // ZERO WIDTH NO-BREAK SPACE (BOM)
    ('\u{fff9}', '\u{fffb}'),   // INTERLINEAR ANNOTATION ANCHOR ..
    ('\u{110bd}', '\u{110bd}'), // KAITHI NUMBER SIGN
    ('\u{110cd}', '\u{110cd}'), // KAITHI NUMBER SIGN ABOVE
    ('\u{13430}', '\u{1343f}'), // EGYPTIAN HIEROGLYPH format controls
    ('\u{1bca0}', '\u{1bca3}'), // SHORTHAND FORMAT LETTER OVERLAP ..
    ('\u{1d173}', '\u{1d17a}'), // MUSICAL SYMBOL BEGIN BEAM ..
    ('\u{e0001}', '\u{e0001}'), // LANGUAGE TAG
    ('\u{e0020}', '\u{e007f}'), // TAG SPACE .. CANCEL TAG
];

/// Characters that reorder the text around them while occupying no width,
/// under the Unicode bidi algorithm (ICU, VTE, iTerm2 3.5+, browsers,
/// spreadsheets). comfy-table never appends a terminating PDF or PDI, so one
/// U+202E reverses the rest of the row.
#[cfg(test)]
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

/// Zero-width format characters that hide rather than reorder: a SAN logged
/// as `gi\u{200b}thub.com` renders as `github.com` pixel for pixel, and pastes
/// as something else.
#[cfg(test)]
const ZERO_WIDTH_FORMATS: &[char] = &[
    '\u{200b}', // ZERO WIDTH SPACE
    '\u{200c}', // ZERO WIDTH NON-JOINER
    '\u{200d}', // ZERO WIDTH JOINER
    '\u{2060}', // WORD JOINER
    '\u{feff}', // BYTE ORDER MARK
];

/// Whether a character takes width away without giving anything back: it
/// reorders the text around it, hides inside it, or breaks it. See
/// [`FORMAT_RANGES`].
fn is_format_control(c: char) -> bool {
    FORMAT_RANGES
        .iter()
        .any(|&(first, last)| (first..=last).contains(&c))
}

/// Whether a character has to be rendered as an escape rather than emitted.
///
/// `is_control()` is Cc, which already subsumes the C1 range U+0080..=U+009F
/// this used to re-test separately — an exhaustive scan over every Unicode
/// scalar finds nothing satisfying that range without also being a control.
fn needs_escaping(c: char) -> bool {
    c != '\n' && (c.is_control() || is_format_control(c))
}

/// How an escaped character is rendered, shared by both sanitisers so the
/// table and the CSV spell the same character the same way.
fn escaped(c: char) -> String {
    format!("\\u{{{:04x}}}", c as u32)
}

/// Replace control characters that a terminal would act on rather than print.
///
/// comfy-table measures a cell in bytes it believes are printable, so an ANSI
/// escape in a certificate identity counts as width and gets split mid-sequence
/// by wrapping: the reset lands on another line and the colour leaks into the
/// session. A CR is worse — it overwrites the line just drawn.
///
/// U+000A is left alone: comfy-table wraps on it correctly, and it is the one
/// control character that means something here.
///
/// Table paths only. `serde_json` escapes C0, `"` and `\` but not DEL or C1,
/// which survive `--json` raw deliberately: JSON consumers decode escapes
/// anyway, and running this over it would emit `\u{009b}` whose backslash
/// serde escapes a second time, corrupting a documented contract.
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
    fn format_control_safe_leaves_everything_else_to_the_csv_writer() {
        assert_eq!(format_control_safe("a,b"), "a,b");
        assert_eq!(format_control_safe("two\nlines"), "two\nlines");
        assert_eq!(format_control_safe("quote\"inside"), "quote\"inside");
        assert_eq!(format_control_safe("tab\there"), "tab\there");
    }

    /// The zero-width half of the class: U+200B renders as nothing at any
    /// terminal font, so `gi\u{200b}thub.com` displayed as `github.com` and
    /// pasted as something else. Both formats now show the escape instead.
    #[test]
    fn zero_width_format_characters_are_escaped_in_both_formats() {
        for c in ZERO_WIDTH_FORMATS {
            let hostile = format!("gi{c}thub.com");
            let table = display_safe(&hostile);
            assert!(
                !table.contains(*c),
                "U+{:04X} reached the table verbatim: {table:?}",
                *c as u32
            );
            let safe = csv_safe(&hostile);
            assert!(
                !safe.contains(*c),
                "U+{:04X} reached the CSV verbatim: {safe:?}",
                *c as u32
            );
        }
    }

    /// The look-alikes the hand-written lists missed: each is category Cf (or
    /// a line separator), renders as nothing in a terminal, and turned
    /// `example.com` into a certificate that displayed as it while matching
    /// something else.
    #[test]
    fn format_characters_outside_the_old_lists_are_escaped_in_both_formats() {
        for c in [
            '\u{00ad}',  // SOFT HYPHEN
            '\u{180e}',  // MONGOLIAN VOWEL SEPARATOR
            '\u{2028}',  // LINE SEPARATOR
            '\u{2029}',  // PARAGRAPH SEPARATOR
            '\u{2061}',  // FUNCTION APPLICATION
            '\u{2064}',  // INVISIBLE PLUS
            '\u{206f}',  // NOMINAL DIGIT SHAPES
            '\u{e0001}', // LANGUAGE TAG
            '\u{e0041}', // TAG LATIN CAPITAL LETTER A
            '\u{e007f}', // CANCEL TAG
        ] {
            let hostile = format!("exa{c}mple.com");
            let table = display_safe(&hostile);
            assert!(
                !table.contains(c),
                "U+{:04X} reached the table verbatim: {table:?}",
                c as u32
            );
            assert!(table.contains(&escaped(c)), "{table:?}");
            let csv = csv_safe(&hostile);
            assert!(
                !csv.contains(c),
                "U+{:04X} reached the CSV verbatim: {csv:?}",
                c as u32
            );
        }
    }

    /// The named cases stay inside the category rule that replaced them, so
    /// regenerating the table can never quietly drop one.
    #[test]
    fn the_named_bidi_and_zero_width_cases_are_inside_the_ranges() {
        for c in BIDI_CONTROLS.iter().chain(ZERO_WIDTH_FORMATS) {
            assert!(is_format_control(*c), "U+{:04X} left the table", *c as u32);
        }
    }

    /// Ranges are written by hand from a generated list, so check they are
    /// well formed and in order — an inverted pair would match nothing.
    #[test]
    fn the_format_ranges_are_ordered_and_disjoint() {
        for (first, last) in FORMAT_RANGES {
            assert!(
                first <= last,
                "U+{:04X} > U+{:04X}",
                *first as u32,
                *last as u32
            );
        }
        for pair in FORMAT_RANGES.windows(2) {
            assert!(
                pair[0].1 < pair[1].0,
                "{pair:?} overlap or are out of order"
            );
        }
    }

    /// Ordinary text next to the ranges is untouched: the plain hyphen and
    /// space a DN is full of, and the letters either side of each block.
    #[test]
    fn neighbours_of_the_format_ranges_are_left_alone() {
        for ordinary in [
            "-", " ", "\u{00ac}", "\u{00ae}", "\u{2027}", "\u{2030}", "\u{2065}",
        ] {
            assert_eq!(display_safe(ordinary), ordinary);
            assert_eq!(format_control_safe(ordinary), ordinary);
        }
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
