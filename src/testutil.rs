//! Fixtures shared by the unit tests in more than one module.
//!
//! Compiled only under `cfg(test)`; see `main.rs`.

use chrono::{DateTime, NaiveDate, Utc};

/// Midnight UTC on the given date. Every module that builds a `SearchRow` or
/// `RawRow` by hand needs one, and three private copies had already drifted
/// into three identical functions.
pub fn utc(y: i32, m: u32, d: u32) -> DateTime<Utc> {
    NaiveDate::from_ymd_opt(y, m, d)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
}
