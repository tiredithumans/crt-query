mod cli;
mod config;
mod db;
mod output;
mod queries;
mod update;

use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, Commands};
use crate::db::Db;
use crate::queries::cert::CertDetail;

/// Completed; results were emitted, even if there were none.
const EXIT_OK: i32 = 0;
/// The run failed.
const EXIT_ERROR: i32 = 1;
/// The requested certificate ID does not exist. Distinct from EXIT_ERROR so a
/// script can tell "no such certificate" from "the query failed".
///
/// 3 rather than the more obvious 2: clap exits 2 on a usage error, so
/// `crt-query cert "$id"` with an empty or unset `$id` would otherwise report
/// "no such certificate" for what is a typo — turning a shell slip into a
/// false "the certificate is gone" alert, which is the exact confusion this
/// code exists to prevent.
const EXIT_NOT_FOUND: i32 = 3;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    match run().await {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("Error: {e:#}");
            std::process::exit(EXIT_ERROR);
        }
    }
}

async fn run() -> Result<i32> {
    let cli = Cli::parse();
    // Check the CSV destination before any real work: before a connection is
    // spent on the shared guest database, which is a genuinely scarce
    // resource, and before check-update's network round trip.
    //
    // `completions` is exempt: it emits a shell script, never a record, so the
    // output flags do not apply to it and touching the path would only leave a
    // file behind for a run that was never going to write one.
    if !matches!(cli.command, Commands::Completions { .. }) {
        output::precheck_csv(&cli.out)?;
    }

    match &cli.command {
        Commands::Search {
            query,
            valid_since,
            all_history,
            skip_expired,
            limit,
            no_dedupe,
        } => {
            let terms = Commands::unique_terms(query);
            let lookback = Commands::search_lookback(*valid_since, *all_history);
            let db = open_db(&cli).await?;
            let rows = queries::search::run_search(
                &db,
                &terms,
                lookback,
                *skip_expired,
                *limit,
                !no_dedupe,
            )
            .await?;
            if rows.is_empty() {
                let names = quoted(&terms);
                if *skip_expired {
                    eprintln!("No unexpired certificates found for {names}.");
                } else if lookback == cli::ALL_HISTORY {
                    eprintln!("No certificates found for {names}.");
                } else {
                    eprintln!(
                        "No certificates found for {names} valid within the last \
                         {lookback} day(s); widen with --valid-since or --all-history."
                    );
                }
            }
            // Emitted even when empty: --json still owes the caller `[]`, and
            // --csv still owes a file, or a stale one is silently reused.
            output::emit(&rows, &cli.out)?;
        }
        Commands::Cert { id } => {
            let db = open_db(&cli).await?;
            match queries::cert::run_cert(&db, *id).await? {
                Some(detail) => output::emit_detail(&detail, &cli.out)?,
                None => {
                    eprintln!("No certificate with crt.sh ID {id}.");
                    output::emit_missing::<CertDetail>(&cli.out, EXIT_NOT_FOUND)?;
                    return Ok(EXIT_NOT_FOUND);
                }
            }
        }
        Commands::Expiring {
            domain,
            within,
            since_expired,
            skip_expired,
            limit,
            no_dedupe,
        } => {
            let domains = Commands::unique_terms(domain);
            let lookback = Commands::expiring_lookback(*since_expired, *skip_expired);
            let db = open_db(&cli).await?;
            let rows = queries::expiring::run_expiring(
                &db, &domains, *within, lookback, *limit, !no_dedupe,
            )
            .await?;
            if rows.is_empty() {
                let names = quoted(&domains);
                let limit_note = limit_note(&domains);
                if lookback == 0 {
                    eprintln!(
                        "No unexpired certificates for {names} expiring within \
                         {within} day(s) ({limit_note})."
                    );
                } else {
                    eprintln!(
                        "No certificates for {names} expiring within {within} day(s) \
                         or expired in the last {lookback} day(s) ({limit_note})."
                    );
                }
            }
            output::emit(&rows, &cli.out)?;
        }
        // Neither of the following needs the database.
        Commands::CheckUpdate => update::run_check_update(&cli.out)?,
        Commands::Completions { shell } => {
            // Rendered to memory first, then written through the same stdout
            // path as every other output: clap_complete's generate() panics on
            // a write error, so handing it a raw stdout makes
            // `crt-query completions bash | head -1` an exit-101 panic rather
            // than the clean end of output it is everywhere else.
            let mut script = Vec::new();
            cli::write_completions(*shell, &mut script);
            output::emit_raw(&script)?;
        }
    }
    Ok(EXIT_OK)
}

/// Resolve the connection settings — CLI flags over config file over built-in
/// defaults — and connect.
///
/// Called from inside the subcommand arms rather than once up front, so that
/// `completions` and `check-update` neither read the config file nor open a
/// connection to a shared public service they have no use for.
async fn open_db(cli: &Cli) -> Result<Db> {
    let file = config::load()?;
    db::connect(&config::resolve(&cli.conn, &file)).await
}

/// How to name the requested terms in an empty-result message.
fn quoted(terms: &[String]) -> String {
    terms
        .iter()
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The `--limit` caveat, which reads differently once more than one domain is
/// in play because the limit applies to each of them.
fn limit_note(domains: &[String]) -> &'static str {
    if domains.len() == 1 {
        "in the first --limit rows"
    } else {
        "in the first --limit rows per domain"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// clap exits 2 on a usage error and we do not control that number, so the
    /// only way "no such certificate" stays distinguishable from "you typed the
    /// command wrong" is for none of our codes to be 2.
    #[test]
    fn no_exit_code_collides_with_claps_usage_error() {
        const CLAP_USAGE_ERROR: i32 = 2;
        for (name, code) in [
            ("EXIT_OK", EXIT_OK),
            ("EXIT_ERROR", EXIT_ERROR),
            ("EXIT_NOT_FOUND", EXIT_NOT_FOUND),
        ] {
            assert_ne!(code, CLAP_USAGE_ERROR, "{name} collides with clap's exit 2");
        }
        let mut codes = [EXIT_OK, EXIT_ERROR, EXIT_NOT_FOUND];
        codes.sort_unstable();
        let distinct = codes.windows(2).all(|w| w[0] != w[1]);
        assert!(distinct, "exit codes must stay distinguishable: {codes:?}");
    }

    #[test]
    fn one_domain_reads_as_a_single_limit_window() {
        let domains = ["example.com".to_string()];
        assert_eq!(quoted(&domains), "\"example.com\"");
        assert_eq!(limit_note(&domains), "in the first --limit rows");
    }

    #[test]
    fn several_domains_are_listed_and_the_limit_is_marked_per_domain() {
        let domains = ["a.example".to_string(), "b.example".to_string()];
        assert_eq!(quoted(&domains), "\"a.example\", \"b.example\"");
        assert_eq!(limit_note(&domains), "in the first --limit rows per domain");
    }
}
