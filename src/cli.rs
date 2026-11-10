use std::io::Write;
use std::path::PathBuf;

use clap::{Args, CommandFactory, Parser, Subcommand};
use clap_complete::Shell;

use crate::queries::cert::CertRef;

/// Look-back that disables the `search` validity floor entirely.
pub const ALL_HISTORY: i32 = 0;

/// Query crt.sh certificate-transparency data directly from its public
/// PostgreSQL database (crt.sh:5432, read-only guest access).
#[derive(Parser)]
#[command(name = "crt-query", version, about)]
pub struct Cli {
    // Headed groups, because these flags are global: without a heading clap
    // lists them under a subcommand's own "Options", interleaved with its
    // flags by declaration order, so `search --help` read `--host`,
    // `--valid-since`, `--all-history`, `--port`.
    #[command(flatten, next_help_heading = "Connection")]
    pub conn: ConnOpts,

    #[command(flatten, next_help_heading = "Output")]
    pub out: OutputOpts,

    #[command(flatten, next_help_heading = "Cache")]
    pub cache: CacheOpts,

    #[command(subcommand)]
    pub command: Commands,
}

/// Connection flags. Every field is optional so an unset value falls back to a
/// `CRT_QUERY_*` env var, then the config file, then defaults — see
/// `config::resolve`.
///
/// The env vars are read by `config::EnvConfig`, not clap's `env` attribute
/// (see there for why), so the help text names each one by hand — the
/// variable, never its value, which may carry a password.
#[derive(Args)]
pub struct ConnOpts {
    /// Database host (default: crt.sh; env: CRT_QUERY_HOST)
    #[arg(long, global = true)]
    pub host: Option<String>,

    /// Database port (default: 5432; env: CRT_QUERY_PORT)
    #[arg(long, global = true)]
    pub port: Option<u16>,

    /// Database name (default: certwatch; env: CRT_QUERY_DBNAME)
    #[arg(long, global = true)]
    pub dbname: Option<String>,

    /// Database user (default: guest; env: CRT_QUERY_USER)
    #[arg(long, global = true)]
    pub user: Option<String>,

    /// Full postgres:// URL; overrides --host/--port/--dbname/--user
    /// (env: CRT_QUERY_DB_URL — use that, or the config file, for a URL
    /// carrying a password: a flag shows in `ps` and shell history)
    #[arg(long, global = true, value_name = "URL")]
    pub db_url: Option<String>,
}

#[derive(Args)]
pub struct OutputOpts {
    /// Emit JSON to stdout instead of a table
    #[arg(long, global = true)]
    pub json: bool,

    /// Print nothing on stderr but errors: no progress line, no notes, no
    /// "wrote N CSV row(s)". For scheduled runs, where every line of stderr
    /// becomes mail
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Additionally write results as CSV to this file. `-` writes the CSV to
    /// stdout in place of the table, for piping
    #[arg(long, global = true, value_name = "PATH")]
    pub csv: Option<PathBuf>,

    /// Render the table at exactly this width, with every column. The
    /// automatic layout otherwise fits the terminal, or 120 columns when
    /// stdout is not one, leaving out the least-read columns if it must
    #[arg(
        long,
        global = true,
        value_name = "COLS",
        allow_negative_numbers = true,
        value_parser = clap::value_parser!(u16).range(40..=1000),
    )]
    pub width: Option<u16>,
}

/// Cache flags.
///
/// Results are cached locally so repeating a query does not spend another
/// client slot on a shared public database. See `cache::Cache`.
#[derive(Args)]
pub struct CacheOpts {
    /// Neither read nor write the local result cache
    #[arg(long, global = true, conflicts_with = "refresh")]
    pub no_cache: bool,

    /// Ignore any cached result and re-query, then cache the fresh answer.
    /// Validity windows are evaluated by the server at query time, so a cached
    /// result carries the window as it stood when it was written
    #[arg(long, global = true)]
    pub refresh: bool,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Search certificates by domain or identity (crt.sh-style)
    #[command(after_help = SEARCH_EXAMPLES)]
    Search {
        /// Domains or identities to search for. `%` and `_` act as wildcards
        /// in the substring match, but the full-text identity predicate has
        /// to match too and reads them as word breaks, so a term of only
        /// wildcards finds nothing.
        /// Several may be given; --limit applies per term, and results are
        /// merged (and deduplicated) across all of them
        #[arg(required = true, num_args = 1.., value_parser = normalised_term)]
        query: Vec<String>,

        /// Only consider certificates still valid within this many days of
        /// now; bounds the server-side window so --limit is not spent on
        /// long-expired certificates
        #[arg(
            long,
            default_value_t = 365,
            value_name = "DAYS",
            allow_negative_numbers = true,
            value_parser = clap::value_parser!(i32).range(1..=36_500),
        )]
        valid_since: i32,

        /// Search the full history instead, with no validity floor
        #[arg(long, conflicts_with = "valid_since")]
        all_history: bool,

        /// Only certificates that are still valid; excludes anything already
        /// expired. Stricter than --valid-since, and composes with
        /// --all-history to mean "everything crt.sh holds that is live today"
        #[arg(long)]
        skip_expired: bool,

        /// Max identity rows fetched server-side; the deduplicated
        /// certificate count may be lower
        #[arg(
            long,
            default_value_t = 100,
            value_name = "ROWS",
            allow_negative_numbers = true,
            value_parser = clap::value_parser!(i64).range(1..=100_000),
        )]
        limit: i64,

        /// Show raw rows: one per matched identity, precertificate/leaf
        /// pairs not collapsed
        #[arg(long)]
        no_dedupe: bool,
    },

    /// Show full details for one certificate, by crt.sh ID or SHA-256
    /// fingerprint
    #[command(after_help = CERT_EXAMPLES)]
    Cert {
        /// crt.sh certificate ID, or the certificate's SHA-256 fingerprint
        /// (64 hex digits; colons, as openssl prints them, are allowed)
        #[arg(value_name = "ID|SHA256", value_parser = CertRef::parse)]
        lookup: CertRef,
    },

    /// Report expired or soon-expiring certificates for one or more domains
    #[command(after_help = EXPIRING_EXAMPLES)]
    Expiring {
        /// Domains to check; --limit applies per domain, and results are
        /// merged (and deduplicated) across all of them
        #[arg(required = true, num_args = 1.., value_parser = normalised_term)]
        domain: Vec<String>,

        /// Look-ahead window in days
        #[arg(
            long,
            default_value_t = 30,
            value_name = "DAYS",
            allow_negative_numbers = true,
            value_parser = clap::value_parser!(i32).range(0..=36_500),
        )]
        within: i32,

        /// How far back to include already-expired certificates; bounds the
        /// server-side window so --limit is not spent on ancient rows
        #[arg(
            long,
            default_value_t = 30,
            value_name = "DAYS",
            allow_negative_numbers = true,
            value_parser = clap::value_parser!(i32).range(0..=36_500),
        )]
        since_expired: i32,

        /// Exclude certificates that have already expired
        /// (equivalent to --since-expired 0). Needs a --within of at least 1
        #[arg(long, conflicts_with = "since_expired")]
        skip_expired: bool,

        /// Max identity rows fetched server-side; the deduplicated
        /// certificate count may be lower
        #[arg(
            long,
            default_value_t = 500,
            value_name = "ROWS",
            allow_negative_numbers = true,
            value_parser = clap::value_parser!(i64).range(1..=100_000),
        )]
        limit: i64,

        /// Show raw rows: one per matched identity, precertificate/leaf
        /// pairs not collapsed
        #[arg(long)]
        no_dedupe: bool,

        /// Leave out a certificate when every name it matched is also on a
        /// certificate that outlives the --within window: it has been
        /// renewed, and is expiring on schedule. Costs a second query per
        /// domain
        #[arg(long)]
        ignore_renewed: bool,

        /// Exit 4 instead of 0 when the report lists any certificate, so cron
        /// or a monitoring check can act on the status alone. The report is
        /// printed in full either way
        #[arg(long)]
        fail_on_expiring: bool,
    },

    /// Inspect or clear the local result cache
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },

    /// Report whether a newer release is available. Opt-in by design: no
    /// other subcommand makes a network call to GitHub
    CheckUpdate,

    /// Generate a shell completion script on stdout
    #[command(after_help = COMPLETIONS_EXAMPLES)]
    Completions {
        /// Shell to generate completions for
        #[arg(value_enum)]
        shell: Shell,
    },
}

// Examples, because the flags that matter most are the ones nobody finds by
// reading a list: the cron form of `expiring`, the fingerprint form of `cert`,
// and where a completion script goes. Kept short of 80 columns so wrapping
// never reflows a command line.
const SEARCH_EXAMPLES: &str = "\
Examples:
  crt-query search example.com
  crt-query search example.com example.org --skip-expired
  crt-query search example.com --all-history --limit 1000 --json";

const CERT_EXAMPLES: &str = "\
Examples:
  crt-query cert 1234567890
  crt-query cert \"$(openssl x509 -in cert.pem -noout -fingerprint -sha256 \\
      | cut -d= -f2)\"";

const EXPIRING_EXAMPLES: &str = "\
Examples:
  crt-query expiring example.com --within 14
  crt-query expiring example.com example.org --skip-expired --csv report.csv

  In cron, where exit 4 means a certificate is running out unrenewed:
  crt-query --quiet expiring example.com --skip-expired --ignore-renewed \\
      --fail-on-expiring";

const COMPLETIONS_EXAMPLES: &str = "\
Examples:
  zsh (needs `fpath+=(~/.zfunc)` before `compinit` in ~/.zshrc):
    mkdir -p ~/.zfunc && crt-query completions zsh > ~/.zfunc/_crt-query
  bash:
    mkdir -p ~/.local/share/bash-completion/completions
    crt-query completions bash \\
      > ~/.local/share/bash-completion/completions/crt-query
  fish:
    crt-query completions fish > ~/.config/fish/completions/crt-query.fish
  PowerShell (add to $PROFILE):
    crt-query completions powershell | Out-String | Invoke-Expression";

/// What `crt-query cache` should do.
#[derive(Subcommand, Clone, Copy)]
pub enum CacheAction {
    /// Print where cached results are stored
    Path,
    /// Delete every cached result
    Clear,
}

impl Commands {
    /// Days of look-back for `search`, or [`ALL_HISTORY`] for no floor.
    pub fn search_lookback(valid_since: i32, all_history: bool) -> i32 {
        if all_history {
            ALL_HISTORY
        } else {
            valid_since
        }
    }

    /// Days of look-back for `expiring`. `--skip-expired` is exactly a
    /// zero-day look-back, which keeps one query serving both modes.
    pub fn expiring_lookback(since_expired: i32, skip_expired: bool) -> i32 {
        if skip_expired { 0 } else { since_expired }
    }

    /// A `search` or `expiring` term list with repeats removed, order kept.
    ///
    /// Each term costs one statement against a shared public database, and
    /// identity matching is case-insensitive, so `a.example A.example` would
    /// otherwise buy two identical result sets for twice the load.
    pub fn unique_terms(terms: &[String]) -> Vec<String> {
        let mut seen = Vec::with_capacity(terms.len());
        let mut out = Vec::with_capacity(terms.len());
        for term in terms {
            let key = term.to_lowercase();
            if !seen.contains(&key) {
                seen.push(key);
                out.push(term.clone());
            }
        }
        out
    }
}

/// A `search` term or `expiring` domain, tidied and checked at parse time.
///
/// An empty term still costs a statement against the shared guest database
/// (`plainto_tsquery` of nothing matches no row), and usually comes from an
/// unset shell variable. "No certificates" is a result people act on, so a
/// slip must read as a usage error — the same reasoning that keeps
/// `EXIT_NOT_FOUND` off clap's 2.
///
/// The same reasoning covers what a term picks up on its way here. Terms used
/// to be bound exactly as typed, so `example.com\r` — every line `xargs` reads
/// from a CRLF file — became the pattern `%example.com\r%`, matched nothing,
/// and reported "No certificates found" with the terminal swallowing the one
/// character that explained it. So:
///
/// - surrounding whitespace is trimmed;
/// - a control character anywhere else is refused, since no identity holds one;
/// - one trailing dot is dropped from a term with no whitespace in it: the
///   fully-qualified spelling `example.com.` cannot match the identity
///   `example.com`. A term with whitespace is a DN fragment such as
///   `O=Example, Inc.`, where the dot is the caller's to keep.
///
/// Whitespace inside a term is left alone.
fn normalised_term(term: &str) -> Result<String, String> {
    const EMPTY: &str = "a term cannot be empty or only whitespace";
    let term = term.trim();
    if term.is_empty() {
        return Err(EMPTY.to_string());
    }
    if let Some(c) = term.chars().find(|c| c.is_control()) {
        return Err(format!(
            "a term cannot contain a control character (found {})",
            c.escape_default()
        ));
    }
    let term = if term.contains(char::is_whitespace) {
        term
    } else {
        term.strip_suffix('.').unwrap_or(term)
    };
    if term.is_empty() {
        return Err(EMPTY.to_string());
    }
    Ok(term.to_string())
}

impl Cli {
    /// Reject flag combinations clap's attributes cannot express, as the same
    /// usage error (exit 2) clap gives for the ones it can.
    ///
    /// `expiring --within 0 --skip-expired` asks for certificates expiring no
    /// later than now and no earlier than now: the window is a single instant
    /// of the server's microsecond clock, so nothing can ever match and
    /// `--fail-on-expiring` could never fire. A check that cannot alert is
    /// worse than one that fails to start.
    pub fn validate(&self) -> Result<(), clap::Error> {
        // Both claim stdout, and a document that is half of each is neither.
        if self.out.json && crate::output::csv_on_stdout(&self.out) {
            return Err(Self::command().error(
                clap::error::ErrorKind::ArgumentConflict,
                "--csv - and --json both write to stdout; give --csv a file, or drop one",
            ));
        }
        if let Commands::Expiring {
            within: 0,
            skip_expired: true,
            ..
        } = self.command
        {
            return Err(Self::command().error(
                clap::error::ErrorKind::ArgumentConflict,
                "--within 0 with --skip-expired can never match a certificate; \
                 use --within 1 or more, or drop --skip-expired",
            ));
        }
        Ok(())
    }
}

/// Write a completion script for `shell` to `out`.
///
/// The generated script is derived from the same [`Cli`] definition clap
/// parses with, so completions cannot drift from the flags they describe.
pub fn write_completions(shell: Shell, out: &mut dyn Write) {
    let mut command = Cli::command();
    let name = command.get_name().to_string();
    clap_complete::generate(shell, &mut command, name, out);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn all_history_overrides_the_validity_floor() {
        assert_eq!(Commands::search_lookback(365, true), ALL_HISTORY);
        assert_eq!(Commands::search_lookback(365, false), 365);
    }

    #[test]
    fn skip_expired_is_a_zero_day_lookback() {
        assert_eq!(Commands::expiring_lookback(30, true), 0);
        assert_eq!(Commands::expiring_lookback(90, false), 90);
    }

    #[test]
    fn duplicate_terms_are_collapsed_case_insensitively() {
        let given = ["a.example", "A.EXAMPLE", "b.example", "a.example"].map(String::from);
        assert_eq!(
            Commands::unique_terms(&given),
            vec!["a.example".to_string(), "b.example".to_string()],
            "the first spelling given should survive, in order"
        );
    }

    #[test]
    fn expiring_accepts_several_domains() {
        let cli = Cli::try_parse_from(["crt-query", "expiring", "a.example", "b.example"]).unwrap();
        let Commands::Expiring { domain, .. } = cli.command else {
            panic!("expected the expiring subcommand");
        };
        assert_eq!(
            domain,
            vec!["a.example".to_string(), "b.example".to_string()]
        );
    }

    #[test]
    fn expiring_needs_at_least_one_domain() {
        assert!(Cli::try_parse_from(["crt-query", "expiring"]).is_err());
    }

    #[test]
    fn search_accepts_several_terms() {
        let cli = Cli::try_parse_from(["crt-query", "search", "a.example", "b.example"]).unwrap();
        let Commands::Search { query, .. } = cli.command else {
            panic!("expected the search subcommand");
        };
        assert_eq!(
            query,
            vec!["a.example".to_string(), "b.example".to_string()]
        );
    }

    #[test]
    fn search_needs_at_least_one_term() {
        assert!(Cli::try_parse_from(["crt-query", "search"]).is_err());
    }

    /// An unset `$DOMAIN` used to reach the database as an empty term and
    /// come back as "No certificates found". It is a usage error, for both
    /// subcommands and anywhere in the list.
    #[test]
    fn a_blank_term_is_a_usage_error_before_anything_is_queried() {
        for subcommand in ["search", "expiring"] {
            for blank in ["", " ", "\t", " \n "] {
                for args in [
                    vec!["crt-query", subcommand, blank],
                    vec!["crt-query", subcommand, "a.example", blank],
                ] {
                    let Err(err) = Cli::try_parse_from(&args) else {
                        panic!("accepted {args:?}");
                    };
                    assert_eq!(
                        err.kind(),
                        clap::error::ErrorKind::ValueValidation,
                        "{args:?}"
                    );
                }
            }
        }
    }

    /// Whitespace inside a term is the caller's to keep; whitespace around it
    /// is how a term arrives from `xargs` or a CRLF file, and used to be bound
    /// into the pattern, where it matched nothing.
    #[test]
    fn a_term_is_trimmed_but_whitespace_inside_it_is_kept() {
        let cli = Cli::try_parse_from([
            "crt-query",
            "search",
            " example.com ",
            "example.org\r",
            "O=Example, Inc.",
        ])
        .unwrap();
        let Commands::Search { query, .. } = cli.command else {
            panic!("expected the search subcommand");
        };
        assert_eq!(
            query,
            vec![
                "example.com".to_string(),
                "example.org".to_string(),
                "O=Example, Inc.".to_string()
            ]
        );
    }

    /// `example.com.` is how a fully-qualified name is written, and no
    /// identity is stored that way. A DN fragment keeps its full stop.
    #[test]
    fn one_trailing_dot_is_dropped_from_a_hostname_but_not_a_dn_fragment() {
        assert_eq!(normalised_term("example.com.").unwrap(), "example.com");
        assert_eq!(normalised_term("example.com..").unwrap(), "example.com.");
        assert_eq!(
            normalised_term("O=Example, Inc.").unwrap(),
            "O=Example, Inc."
        );
        assert!(
            normalised_term(".").is_err(),
            "nothing is left to search for"
        );
    }

    #[test]
    fn a_control_character_inside_a_term_is_a_usage_error() {
        for bad in [
            "exam\tple.com",
            "a.example\r\nb.example",
            "example\u{7}.com",
        ] {
            let Err(err) = Cli::try_parse_from(["crt-query", "search", bad]) else {
                panic!("accepted {bad:?}");
            };
            assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
        }
    }

    #[test]
    fn csv_on_stdout_and_json_cannot_share_it() {
        let parse = |args: &[&str]| Cli::try_parse_from(args).unwrap().validate();
        let err = parse(&["crt-query", "--json", "--csv", "-", "search", "a.example"])
            .expect_err("two documents on one stdout were accepted");
        assert_eq!(err.exit_code(), 2);
        parse(&["crt-query", "--csv", "-", "search", "a.example"]).unwrap();
        parse(&[
            "crt-query",
            "--json",
            "--csv",
            "out.csv",
            "search",
            "a.example",
        ])
        .unwrap();
    }

    /// Help is wrapped to the terminal, 100 columns at most when there is none
    /// to measure. Unwrapped, one paragraph of `search --help` ran past 300.
    #[test]
    fn help_text_is_wrapped_and_carries_examples() {
        let mut command = Cli::command();
        command.build();
        for name in ["search", "cert", "expiring", "completions"] {
            let help = command
                .find_subcommand_mut(name)
                .expect("subcommand exists")
                .render_long_help()
                .to_string();
            assert!(help.contains("Examples:"), "{name} has no examples");
            let widest = help.lines().map(|l| l.chars().count()).max().unwrap();
            assert!(widest <= 100, "{name} help has a {widest}-column line");
        }
    }

    /// The window `--within 0 --skip-expired` describes is one instant wide,
    /// so the run could only ever report an all-clear.
    #[test]
    fn a_window_nothing_can_fall_in_is_a_usage_error() {
        let parse = |args: &[&str]| Cli::try_parse_from(args).unwrap().validate();
        let err = parse(&[
            "crt-query",
            "expiring",
            "a.example",
            "--within",
            "0",
            "--skip-expired",
        ])
        .expect_err("an empty window was accepted");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
        assert_eq!(err.exit_code(), 2);
        // Each half is meaningful on its own.
        parse(&["crt-query", "expiring", "a.example", "--within", "0"]).unwrap();
        parse(&["crt-query", "expiring", "a.example", "--skip-expired"]).unwrap();
        parse(&["crt-query", "search", "a.example", "--skip-expired"]).unwrap();
    }

    #[test]
    fn search_skip_expired_composes_with_the_window_flags() {
        // Stricter than --valid-since rather than contradicting it, and
        // meaningful alongside --all-history, so neither is a conflict.
        for args in [
            vec!["crt-query", "search", "a.example", "--skip-expired"],
            vec![
                "crt-query",
                "search",
                "a.example",
                "--skip-expired",
                "--valid-since",
                "30",
            ],
            vec![
                "crt-query",
                "search",
                "a.example",
                "--skip-expired",
                "--all-history",
            ],
        ] {
            let cli = Cli::try_parse_from(&args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
            let Commands::Search { skip_expired, .. } = cli.command else {
                panic!("expected the search subcommand");
            };
            assert!(skip_expired, "{args:?}");
        }
    }

    #[test]
    fn completions_are_generated_for_every_supported_shell() {
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish, Shell::PowerShell] {
            let mut out = Vec::new();
            write_completions(shell, &mut out);
            let script = String::from_utf8(out).expect("completion scripts are UTF-8");
            assert!(!script.is_empty(), "{shell} produced nothing");
            assert!(
                script.contains("crt-query"),
                "{shell} does not name the binary"
            );
        }
    }

    #[test]
    fn rejects_out_of_range_limits() {
        for bad in [
            vec!["crt-query", "search", "example.com", "--limit", "0"],
            vec!["crt-query", "search", "example.com", "--limit", "-5"],
            vec!["crt-query", "expiring", "example.com", "--within", "-9"],
            vec!["crt-query", "expiring", "example.com", "--limit", "0"],
        ] {
            assert!(Cli::try_parse_from(&bad).is_err(), "accepted {bad:?}");
        }
    }

    /// A negative day count or limit is out of range, and should say so. clap
    /// otherwise reads `-1` as an unknown flag and suggests `-- -1`, which
    /// would pass it as a search term.
    #[test]
    fn a_negative_number_is_a_range_error_not_an_unknown_flag() {
        for args in [
            vec!["crt-query", "expiring", "a.example", "--within", "-1"],
            vec![
                "crt-query",
                "expiring",
                "a.example",
                "--since-expired",
                "-1",
            ],
            vec!["crt-query", "search", "a.example", "--valid-since", "-1"],
            vec!["crt-query", "search", "a.example", "--limit", "-5"],
            vec!["crt-query", "--width", "-80", "search", "a.example"],
        ] {
            let Err(err) = Cli::try_parse_from(&args) else {
                panic!("accepted {args:?}");
            };
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::ValueValidation,
                "{args:?}"
            );
        }
    }

    /// Global flags sit under their own headings, not interleaved with a
    /// subcommand's flags.
    #[test]
    fn global_flags_are_grouped_under_headings_in_subcommand_help() {
        let mut command = Cli::command();
        // Globals are copied into subcommands only when the command is built.
        command.build();
        let search = command
            .find_subcommand_mut("search")
            .expect("search exists");
        let help = search.render_long_help().to_string();
        let at = |needle: &str| {
            help.find(needle)
                .unwrap_or_else(|| panic!("{needle} missing:\n{help}"))
        };
        assert!(at("Connection:") < at("--host"), "{help}");
        assert!(at("--skip-expired") < at("Connection:"), "{help}");
        assert!(
            at("Output:") < at("--json") && at("Cache:") < at("--no-cache"),
            "{help}"
        );
    }

    #[test]
    fn rejects_contradictory_window_flags() {
        assert!(
            Cli::try_parse_from([
                "crt-query",
                "search",
                "example.com",
                "--all-history",
                "--valid-since",
                "30"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "crt-query",
                "expiring",
                "example.com",
                "--skip-expired",
                "--since-expired",
                "30"
            ])
            .is_err()
        );
    }
}
