//! Informational messages on stderr, and the one switch that silences them.
//!
//! Every non-error stderr line — empty-result explanations, the `--limit`
//! saturation note, `wrote N CSV row(s)`, progress lines — goes through
//! [`notice!`], and `--quiet` turns them off at once: cron mails whatever a
//! job writes to stderr, so a successful run produced mail every time. Errors
//! never go through here; `main` prints them unconditionally.
//!
//! A process-wide flag rather than a threaded parameter: it is set once before
//! any call, and passing a boolean through the connection and cache layers to
//! reach one `eprintln!` would couple them to a presentation concern.

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
