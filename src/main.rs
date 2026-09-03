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
const EXIT_NOT_FOUND: i32 = 2;

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
    output::precheck_csv(&cli.out)?;

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
                    output::emit_missing::<CertDetail>(&cli.out)?;
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
        Commands::Completions { shell } => cli::write_completions(*shell, &mut std::io::stdout()),
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
