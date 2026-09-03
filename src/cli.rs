use std::io::Write;
use std::path::PathBuf;

use clap::{Args, CommandFactory, Parser, Subcommand};
use clap_complete::Shell;

/// Look-back that disables the `search` validity floor entirely.
pub const ALL_HISTORY: i32 = 0;

/// Query crt.sh certificate-transparency data directly from its public
/// PostgreSQL database (crt.sh:5432, read-only guest access).
#[derive(Parser)]
#[command(name = "crt-query", version, about)]
pub struct Cli {
    #[command(flatten)]
    pub conn: ConnOpts,

    #[command(flatten)]
    pub out: OutputOpts,

    #[command(subcommand)]
    pub command: Commands,
}

/// Connection flags. Every field is optional so that a value left unset on the
/// command line can fall back to the config file, then to the built-in
/// defaults — see `config::resolve`.
#[derive(Args)]
pub struct ConnOpts {
    /// Database host (default: crt.sh)
    #[arg(long, global = true)]
    pub host: Option<String>,

    /// Database port (default: 5432)
    #[arg(long, global = true)]
    pub port: Option<u16>,

    /// Database name (default: certwatch)
    #[arg(long, global = true)]
    pub dbname: Option<String>,

    /// Database user (default: guest)
    #[arg(long, global = true)]
    pub user: Option<String>,

    /// Full postgres:// URL; overrides --host/--port/--dbname/--user
    #[arg(long, global = true, value_name = "URL")]
    pub db_url: Option<String>,
}

#[derive(Args)]
pub struct OutputOpts {
    /// Emit JSON to stdout instead of a table
    #[arg(long, global = true)]
    pub json: bool,

    /// Additionally write results as CSV to this file
    #[arg(long, global = true, value_name = "PATH")]
    pub csv: Option<PathBuf>,

    /// Render the table at exactly this width. Overrides the column
    /// heuristics used for the automatic layout, which otherwise defaults to
    /// the terminal width, or 120 when stdout is not a terminal
    #[arg(
        long,
        global = true,
        value_name = "COLS",
        value_parser = clap::value_parser!(u16).range(40..=1000),
    )]
    pub width: Option<u16>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Search certificates by domain or identity (crt.sh-style)
    Search {
        /// Domains or identities to search for (% and _ act as wildcards).
        /// Several may be given; --limit applies per term, and results are
        /// merged (and deduplicated) across all of them
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,

        /// Only consider certificates still valid within this many days of
        /// now; bounds the server-side window so --limit is not spent on
        /// long-expired certificates
        #[arg(
            long,
            default_value_t = 365,
            value_name = "DAYS",
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
            value_parser = clap::value_parser!(i64).range(1..=100_000),
        )]
        limit: i64,

        /// Show raw rows: one per matched identity, precertificate/leaf
        /// pairs not collapsed
        #[arg(long)]
        no_dedupe: bool,
    },

    /// Show full details for one certificate by crt.sh ID
    Cert {
        /// crt.sh certificate ID
        id: i64,
    },

    /// Report expired or soon-expiring certificates for one or more domains
    Expiring {
        /// Domains to check; --limit applies per domain, and results are
        /// merged (and deduplicated) across all of them
        #[arg(required = true, num_args = 1..)]
        domain: Vec<String>,

        /// Look-ahead window in days
        #[arg(
            long,
            default_value_t = 30,
            value_parser = clap::value_parser!(i32).range(0..=36_500),
        )]
        within: i32,

        /// How far back to include already-expired certificates; bounds the
        /// server-side window so --limit is not spent on ancient rows
        #[arg(
            long,
            default_value_t = 30,
            value_name = "DAYS",
            value_parser = clap::value_parser!(i32).range(0..=36_500),
        )]
        since_expired: i32,

        /// Exclude certificates that have already expired
        /// (equivalent to --since-expired 0)
        #[arg(long, conflicts_with = "since_expired")]
        skip_expired: bool,

        /// Max identity rows fetched server-side; the deduplicated
        /// certificate count may be lower
        #[arg(
            long,
            default_value_t = 500,
            value_parser = clap::value_parser!(i64).range(1..=100_000),
        )]
        limit: i64,

        /// Show raw rows: one per matched identity, precertificate/leaf
        /// pairs not collapsed
        #[arg(long)]
        no_dedupe: bool,
    },

    /// Report whether a newer release is available. Opt-in by design: no
    /// other subcommand makes a network call to GitHub
    CheckUpdate,

    /// Generate a shell completion script on stdout
    Completions {
        /// Shell to generate completions for
        #[arg(value_enum)]
        shell: Shell,
    },
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

    /// A `search` or `expiring` term list with repeats removed, preserving the
    /// order they were given in.
    ///
    /// Each term costs one statement against a shared public database, and the
    /// identity match is case-insensitive, so `a.example A.example` would
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
