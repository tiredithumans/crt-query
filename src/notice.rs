//! Informational messages on stderr, and the one switch that silences them.
//!
//! Everything this tool says on stderr that is not an error — the empty-result
//! explanations, the `--limit` saturation note, `wrote N CSV row(s)`, the
//! `querying …` progress line, `cache clear`'s count, `check-update`'s upgrade
//! hint — goes through [`notice!`]. `--quiet` turns them all off at once, which
//! is what a scheduled run wants: cron mails whatever a job writes to stderr,
//! so a run that worked fine produced mail every time. Errors never go through
//! here; `main` prints them unconditionally, and a quiet failure is still loud.
//!
//! A process-wide flag rather than a parameter threaded through every call:
//! the messages come from `main`, `output`, `db` and `update`, the flag is set
//! exactly once before any of them run, and passing a boolean through the
//! connection and cache layers only to reach one `eprintln!` would couple them
//! to a presentation concern they otherwise know nothing about.

use std::sync::atomic::{AtomicBool, Ordering};

static QUIET: AtomicBool = AtomicBool::new(false);

/// Silence (or restore) informational messages. Called once, from `main`.
pub fn set_quiet(quiet: bool) {
    QUIET.store(quiet, Ordering::Relaxed);
}

/// Whether informational messages are currently printed.
pub fn enabled() -> bool {
    !QUIET.load(Ordering::Relaxed)
}

/// `eprintln!` for anything that is not an error, suppressed by `--quiet`.
macro_rules! notice {
    ($($arg:tt)*) => {
        if $crate::notice::enabled() {
            eprintln!($($arg)*);
        }
    };
}
pub(crate) use notice;
