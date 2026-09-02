mod cli;
mod db;
mod output;
mod queries;

use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, Commands};
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
    // Check the CSV destination before spending a connection on the shared
    // guest database, which is a genuinely scarce resource.
    output::precheck_csv(&cli.out)?;
    let db = db::connect(&cli.conn).await?;

    match &cli.command {
        Commands::Search {
            query,
            valid_since,
            all_history,
            limit,
            no_dedupe,
        } => {
            let lookback = Commands::search_lookback(*valid_since, *all_history);
            let rows =
                queries::search::run_search(&db, query, lookback, *limit, !no_dedupe).await?;
            if rows.is_empty() {
                if lookback == cli::ALL_HISTORY {
                    eprintln!("No certificates found for \"{query}\".");
                } else {
                    eprintln!(
                        "No certificates found for \"{query}\" valid within the last \
                         {lookback} day(s); widen with --valid-since or --all-history."
                    );
                }
            }
            // Emitted even when empty: --json still owes the caller `[]`, and
            // --csv still owes a file, or a stale one is silently reused.
            output::emit(&rows, &cli.out)?;
        }
        Commands::Cert { id } => match queries::cert::run_cert(&db, *id).await? {
            Some(detail) => output::emit_detail(&detail, &cli.out)?,
            None => {
                eprintln!("No certificate with crt.sh ID {id}.");
                output::emit_missing::<CertDetail>(&cli.out)?;
                return Ok(EXIT_NOT_FOUND);
            }
        },
        Commands::Expiring {
            domain,
            within,
            since_expired,
            skip_expired,
            limit,
            no_dedupe,
        } => {
            let lookback = Commands::expiring_lookback(*since_expired, *skip_expired);
            let rows =
                queries::expiring::run_expiring(&db, domain, *within, lookback, *limit, !no_dedupe)
                    .await?;
            if rows.is_empty() {
                if lookback == 0 {
                    eprintln!(
                        "No unexpired certificates for \"{domain}\" expiring within \
                         {within} day(s) (in the first --limit rows)."
                    );
                } else {
                    eprintln!(
                        "No certificates for \"{domain}\" expiring within {within} day(s) \
                         or expired in the last {lookback} day(s) (in the first --limit rows)."
                    );
                }
            }
            output::emit(&rows, &cli.out)?;
        }
    }
    Ok(EXIT_OK)
}
