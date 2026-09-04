mod cache;
mod cli;
mod config;
mod db;
mod output;
mod queries;
#[cfg(test)]
mod testutil;
mod update;

use std::path::Path;

use anyhow::{Context, Result};
use clap::Parser;

use crate::cache::Cache;
use crate::cli::{CacheAction, Cli, Commands};
use crate::db::Source;
use crate::queries::Report;
use crate::queries::cert::CertDetail;

/// Completed; results were emitted, even if there were none.
///
/// `pub(crate)` because `output.rs` reports it when a reader goes away
/// mid-write; this file still owns the contract.
pub(crate) const EXIT_OK: i32 = 0;
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
            let (mut source, cache) = open_source(&cli)?;
            let report = queries::search::run_search(
                &mut source,
                &cache,
                &terms,
                lookback,
                *skip_expired,
                *limit,
                !no_dedupe,
            )
            .await?;
            if report.rows.is_empty() {
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
            } else if report.window_hid_certificates() {
                eprintln!("{}", saturation_note(*limit, &terms, &report));
            }
            // Emitted even when empty: --json still owes the caller `[]`, and
            // --csv still owes a file, or a stale one is silently reused.
            output::emit(&report.rows, &cli.out)?;
        }
        Commands::Cert { id } => {
            let (mut source, cache) = open_source(&cli)?;
            match queries::cert::run_cert(&mut source, &cache, *id).await? {
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
            let (mut source, cache) = open_source(&cli)?;
            let report = queries::expiring::run_expiring(
                &mut source,
                &cache,
                &domains,
                *within,
                lookback,
                *limit,
                !no_dedupe,
            )
            .await?;
            if report.rows.is_empty() {
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
            } else if report.window_hid_certificates() {
                eprintln!("{}", saturation_note(*limit, &domains, &report));
            }
            output::emit(&report.rows, &cli.out)?;
        }
        // None of the following needs the database.
        Commands::Cache { action } => run_cache(*action)?,
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
/// defaults — and the cache that fronts them.
///
/// Called from inside the subcommand arms rather than once up front, so that
/// `completions` and `check-update` neither read the config file nor open a
/// connection to a shared public service they have no use for.
///
/// Nothing is dialled here. [`Source`] connects on its first real need, so a
/// run whose every term is already cached finishes without touching crt.sh —
/// which is the whole point of having a cache in front of a service that
/// regularly refuses connections.
fn open_source(cli: &Cli) -> Result<(Source, Cache)> {
    let file = config::load()?;
    let cache = build_cache(&cli.cache, &file);
    Ok((Source::new(config::resolve(&cli.conn, &file)), cache))
}

/// Fold the cache flags and config file into a cache.
///
/// Same precedence as everything else here — flag, then file, then default —
/// which means `--refresh` re-enables a cache the file turned off. That is the
/// point of the flag: it asks for a fresh answer to be stored, and honouring
/// `cache = false` over it would make it a slower synonym for `--no-cache`.
fn build_cache(opts: &cli::CacheOpts, file: &config::FileConfig) -> Cache {
    let mode = if opts.no_cache {
        cache::Mode::Disabled
    } else if opts.refresh {
        cache::Mode::Refresh
    } else if file.cache == Some(false) {
        cache::Mode::Disabled
    } else {
        cache::Mode::Enabled
    };
    let ttl = file
        .cache_ttl_secs
        .map_or(cache::DEFAULT_TTL, std::time::Duration::from_secs);
    Cache::new(mode, ttl)
}

/// `crt-query cache path` / `crt-query cache clear`.
///
/// Built with the cache forced on, so `--no-cache` earlier in the command line
/// cannot leave `cache clear` silently clearing nothing.
fn run_cache(action: CacheAction) -> Result<()> {
    let file = config::load()?;
    let ttl = file
        .cache_ttl_secs
        .map_or(cache::DEFAULT_TTL, std::time::Duration::from_secs);
    let cache = Cache::new(cache::Mode::Enabled, ttl);
    let Some(dir) = cache.dir().map(Path::to_path_buf) else {
        // No absolute cache directory in this environment, so there is nowhere
        // for entries to be — see `cache::cache_root` for why relative is
        // refused rather than resolved.
        eprintln!("No cache directory: neither XDG_CACHE_HOME nor HOME names an absolute path.");
        return Ok(());
    };
    match action {
        CacheAction::Path => println!("{}", dir.display()),
        CacheAction::Clear => {
            let removed = cache
                .clear()
                .with_context(|| format!("clearing the cache in {}", dir.display()))?;
            eprintln!("Cleared {removed} cached result(s) from {}.", dir.display());
        }
    }
    Ok(())
}

/// How to name the requested terms in an empty-result message.
fn quoted(terms: &[String]) -> String {
    terms
        .iter()
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The note printed when the row window filled and the collapse then handed
/// back fewer certificates than rows.
///
/// `--limit` bounds identity rows, not certificates: crt.sh returns one row per
/// matched identity per CT entry, and logs a precertificate alongside its final
/// leaf, so several rows routinely collapse into a single certificate. Without
/// this the run is simply short, and a full window reads as "that is all there
/// is" — the one thing it does not mean.
///
/// The counts are run totals, so with several terms they only add up once
/// `--limit` is understood to be per term — hence the parenthetical, and hence
/// naming the terms that actually filled. Left unqualified, a two-term run
/// could report "11 identity rows" under "--limit 10" and send the reader off
/// to reconcile a contradiction that is not there.
fn saturation_note<T>(limit: i64, terms: &[String], report: &Report<T>) -> String {
    // One term is its own answer: naming it would only repeat the command.
    let (per_term, filled) = if terms.len() == 1 {
        (String::new(), String::new())
    } else {
        (
            " (per term)".to_string(),
            format!(" for {}", quoted(&report.saturated)),
        )
    };
    let (raw, certs) = (report.raw_rows, report.rows.len());
    format!(
        "note: --limit {limit}{per_term} filled the server-side row window{filled}; \
         {raw} identity {rows} collapsed to {certs} {cert}. Raise --limit for \
         more, or --no-dedupe for the raw rows.",
        rows = plural(raw, "row"),
        cert = plural(certs, "certificate"),
    )
}

/// English plural, so a one-certificate result does not read "1 certificates".
fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        word.to_string()
    } else {
        format!("{word}s")
    }
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

    /// `saturated` defaults to every term; `note_for_filled` sets it apart.
    fn note_for(limit: i64, terms: &[&str], certs: usize, raw_rows: usize) -> String {
        note_for_filled(limit, terms, terms, certs, raw_rows)
    }

    fn note_for_filled(
        limit: i64,
        terms: &[&str],
        saturated: &[&str],
        certs: usize,
        raw_rows: usize,
    ) -> String {
        let terms: Vec<String> = terms.iter().map(|t| t.to_string()).collect();
        saturation_note(
            limit,
            &terms,
            &Report {
                rows: vec![(); certs],
                raw_rows,
                saturated: saturated.iter().map(|t| t.to_string()).collect(),
            },
        )
    }

    /// The note is the whole feature. It has to name the flag that caused the
    /// shortfall, both counts, and the two ways out — a user who is told only
    /// that the result is short is exactly as stuck as before.
    #[test]
    fn the_saturation_note_names_the_limit_both_counts_and_the_way_out() {
        let note = note_for(10, &["example.com"], 2, 10);
        assert!(note.contains("--limit 10"), "{note}");
        assert!(note.contains("10 identity rows"), "{note}");
        assert!(note.contains("2 certificates"), "{note}");
        assert!(note.contains("--no-dedupe"), "{note}");
    }

    /// `--limit` is per term, so with two names the ceiling being hit is not
    /// the number typed — it is that number over again for each name. Saying
    /// "--limit 10" flat would send someone looking for ten rows that are not
    /// the ones they are missing.
    #[test]
    fn the_saturation_note_says_per_term_only_when_there_is_more_than_one() {
        assert!(
            note_for(10, &["example.com", "example.org"], 3, 20).contains("--limit 10 (per term)"),
        );
        assert!(
            !note_for(10, &["example.com"], 2, 10).contains("per term"),
            "one term is not per-term"
        );
    }

    /// Saturation is per term, and the counts are run totals, so a multi-term
    /// note has to name the term that actually filled — it is the one to raise
    /// the limit for, and telling someone to widen a two-name search without
    /// saying which half is no better than not telling them at all.
    ///
    /// The totals are also why the parenthetical matters: 11 rows reported
    /// under `--limit 10` is a contradiction until the limit is understood to
    /// be per term.
    #[test]
    fn a_multi_term_note_names_only_the_terms_that_filled() {
        let note = note_for_filled(
            10,
            &["busy.example", "quiet.example"],
            &["busy.example"],
            4,
            11,
        );
        assert!(note.contains("--limit 10 (per term)"), "{note}");
        assert!(note.contains("for \"busy.example\""), "{note}");
        assert!(
            !note.contains("quiet.example"),
            "a term that never filled must not be named: {note}"
        );
        assert!(
            note.contains("11 identity rows collapsed to 4 certificates"),
            "{note}"
        );
    }

    /// A single term is its own answer, so naming it would only repeat back the
    /// command that was just typed.
    #[test]
    fn a_single_term_note_does_not_name_the_term() {
        let note = note_for(10, &["example.com"], 2, 10);
        assert!(!note.contains("example.com"), "{note}");
    }

    /// One surviving certificate is the sharpest form of the surprise, and the
    /// likeliest to be read as "there is nothing here".
    #[test]
    fn the_saturation_note_stays_singular_for_one_certificate() {
        let note = note_for(6, &["example.com"], 1, 6);
        assert!(
            note.contains("6 identity rows collapsed to 1 certificate."),
            "{note}"
        );
    }

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
        // The value itself, not just its distinctness: tests/cli.rs asserts the
        // README documents `3` but cannot see this constant (the crate is
        // bin-only), and this test asserted only that the codes differ — so
        // changing it to 4 left both green and the README wrong.
        assert_eq!(
            EXIT_NOT_FOUND, 3,
            "README documents exit 3 for a missing certificate"
        );
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

    fn cache_opts(no_cache: bool, refresh: bool) -> cli::CacheOpts {
        cli::CacheOpts { no_cache, refresh }
    }

    fn file_with_cache(cache: Option<bool>) -> config::FileConfig {
        config::FileConfig {
            cache,
            ..config::FileConfig::default()
        }
    }

    /// Flag over file over default, the same precedence the connection uses.
    #[test]
    fn cache_flags_beat_the_config_file() {
        use crate::cache::Mode;
        for (no_cache, refresh, file, want, why) in [
            (false, false, None, Mode::Enabled, "the default is on"),
            (true, false, None, Mode::Disabled, "--no-cache turns it off"),
            (false, true, None, Mode::Refresh, "--refresh rewrites"),
            (
                false,
                false,
                Some(false),
                Mode::Disabled,
                "the file can turn it off",
            ),
            (
                false,
                false,
                Some(true),
                Mode::Enabled,
                "the file can leave it on",
            ),
            (
                true,
                false,
                Some(true),
                Mode::Disabled,
                "--no-cache beats the file",
            ),
            // Otherwise --refresh would be a slower --no-cache: it asks for a
            // fresh answer to be *stored*.
            (
                false,
                true,
                Some(false),
                Mode::Refresh,
                "--refresh beats the file",
            ),
        ] {
            let got = build_cache(&cache_opts(no_cache, refresh), &file_with_cache(file)).mode();
            assert_eq!(got, want, "{why}");
        }
    }

    #[test]
    fn a_configured_lifetime_is_read_in_seconds() {
        let file = config::FileConfig {
            cache_ttl_secs: Some(90),
            ..config::FileConfig::default()
        };
        let cache = build_cache(&cache_opts(false, false), &file);
        assert_eq!(cache.ttl(), std::time::Duration::from_secs(90));
        let default = build_cache(&cache_opts(false, false), &config::FileConfig::default());
        assert_eq!(default.ttl(), cache::DEFAULT_TTL);
    }
}
