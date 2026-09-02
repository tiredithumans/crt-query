use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

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

#[derive(Args)]
pub struct ConnOpts {
    /// Database host
    #[arg(long, global = true, default_value = "crt.sh")]
    pub host: String,

    /// Database port
    #[arg(long, global = true, default_value_t = 5432)]
    pub port: u16,

    /// Database name
    #[arg(long, global = true, default_value = "certwatch")]
    pub dbname: String,

    /// Database user
    #[arg(long, global = true, default_value = "guest")]
    pub user: String,

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

    /// Table width in columns; defaults to the terminal width, or 120 when
    /// stdout is not a terminal
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
        /// Domain or identity to search for (% and _ act as wildcards)
        query: String,

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

    /// Report expired or soon-expiring certificates for a domain
    Expiring {
        /// Domain to check
        domain: String,

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

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
