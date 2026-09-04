use std::fmt::Write as _;
use std::io::IsTerminal;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio_postgres::config::Host;
use tokio_postgres::error::SqlState;
use tokio_postgres::types::{ToSql, Type};
use tokio_postgres::{Client, Config, NoTls, Row};

use crate::config::Conn;

/// How many times a connection is dialled before the run gives up.
///
/// Five rather than three because the wait between them is now short: the
/// failure this budget exists for is pgbouncer refusing a client slot
/// (`max_client_conn`), which it does instantly and recovers from in well under
/// a second. Under the old flat two-second delay the same wall time bought two
/// extra chances; it now buys four.
const CONNECT_ATTEMPTS: u32 = 5;

/// The wait after the first failed attempt, doubling from there.
const FIRST_RETRY_DELAY: Duration = Duration::from_millis(250);

/// Ceiling on a single wait, so the last attempts do not drift out to a
/// timescale nobody is still watching the terminal for.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(2);

/// Ceiling on the whole connect phase — every attempt and every wait.
///
/// The attempt count alone does not bound the wall time: an attempt that stalls
/// costs `CONNECT_TIMEOUT`, so five of them would be over a minute of silence
/// before the error appears. Retrying stops once this is spent, which caps the
/// phase at this plus the attempt already in flight — 45s.
///
/// That total is the number to keep an eye on rather than this constant. A
/// failed attempt is no longer announced as it happens, so the whole phase is
/// silent, and an unreachable crt.sh is indistinguishable from a hung terminal
/// until it ends. It is deliberately under what the old three-attempt,
/// two-second-delay schedule could spend (~49s): quiet has to buy a wait that
/// is shorter than the one it replaced, not a longer one.
const CONNECT_BUDGET: Duration = Duration::from_secs(30);

/// Ceiling on a single statement, handshake to last row.
///
/// `Config::connect_timeout` covers only `TcpStream::connect` — not the
/// startup and authentication exchange, and not the query itself. A server
/// that accepts the socket and then stops answering leaves the process silent
/// until the two-hour keepalive default notices, which for a scheduled
/// `expiring --csv` means a wedged slot rather than a fast failure into the
/// next run.
///
/// Set well above crt.sh's own statement timeout (~120s) on purpose: the
/// server's `QUERY_CANCELED` names the real problem and suggests a narrower
/// search, so it should be what fires in the ordinary too-broad-query case.
/// This bound is for the case where nothing comes back at all.
const QUERY_TIMEOUT: Duration = Duration::from_secs(180);

/// Ceiling on one connection attempt, including the name resolution and
/// startup exchange that `connect_timeout` leaves uncovered.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// What `tokio-postgres` applies to a single TCP connect.
///
/// Named rather than left inline because it is the one bound in this file that
/// is not the whole story, and the ordering against `CONNECT_TIMEOUT` reads
/// like a guarantee it does not give: the driver applies this *per resolved
/// address*, inside a loop over hosts and another over the addresses each host
/// resolves to, and `lookup_host` is not covered at all. So an attempt against
/// a multi-address host costs up to 10s x N, which is why `CONNECT_TIMEOUT`
/// wraps the whole thing rather than trusting this to bound it.
const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A connected client plus whatever killed its connection task, if anything.
pub struct Db {
    client: Client,
    /// Set by the spawned connection task when the socket dies. The task no
    /// longer prints it itself: an orphaned line can interleave with table
    /// output or trail a run that otherwise looked successful. Instead the
    /// query path consults it, so the failure is reported once, in context.
    conn_err: Arc<OnceLock<String>>,
    /// Host and port for user-facing messages, never the raw `--db-url`.
    target: String,
}

impl Db {
    /// Run a query, translating guest-DB failure modes into actionable errors.
    ///
    /// `subject` is what this statement is looking up — a domain, a search
    /// term, a certificate ID — and appears in the progress hint.
    ///
    /// `query_typed` uses the unnamed prepared statement: crt.sh sits behind a
    /// transaction-pooling pgbouncer, where named prepared statements fail.
    pub async fn query(
        &self,
        subject: &str,
        sql: &str,
        params: &[(&(dyn ToSql + Sync), Type)],
    ) -> Result<Vec<Row>> {
        self.hint(subject);
        match tokio::time::timeout(QUERY_TIMEOUT, self.client.query_typed(sql, params)).await {
            Ok(result) => result.map_err(|e| self.explain(e)),
            Err(_) => Err(anyhow::anyhow!(
                "{} did not answer within {}s; the guest database is shared and \
                 intermittently slow — retry, or narrow the query with a more \
                 specific term or a lower --limit",
                self.target,
                QUERY_TIMEOUT.as_secs()
            )),
        }
    }

    /// Announce a statement on stderr before it is sent.
    ///
    /// The guest database is intermittently slow, so without this a query
    /// that takes several seconds is indistinguishable from a hang — and
    /// `expiring` over several domains sends one statement per domain, where
    /// silence hides how far along it is. Gated on stderr being a terminal so
    /// scheduled runs keep clean logs, and on stderr rather than stdout so it
    /// never lands in a piped table or JSON document.
    fn hint(&self, subject: &str) {
        if std::io::stderr().is_terminal() {
            eprintln!("querying {} for {subject}…", self.target);
        }
    }

    fn explain(&self, err: tokio_postgres::Error) -> anyhow::Error {
        match explain_context(
            err.code(),
            self.conn_err.get().map(String::as_str),
            err.is_closed(),
            &self.target,
        ) {
            Some(context) => anyhow::Error::new(err).context(context),
            None => anyhow::Error::new(err),
        }
    }
}

/// Translate a query failure into something actionable, or `None` to let the
/// raw error stand.
///
/// Split from [`Db::explain`] so it can be tested: `tokio_postgres::Error`
/// exposes no public constructor carrying a SQLSTATE, so every arm below was
/// reachable only from a live server. Returning `None` rather than a fallback
/// string is deliberate — an unmapped SQLSTATE is better read raw than
/// wrapped in a guess.
///
/// A SQLSTATE the server actually sent is authoritative and more specific than
/// anything the connection task can say, so it is checked first; the
/// dead-connection cause explains the no-SQLSTATE case, where the server never
/// answered at all.
fn explain_context(
    code: Option<&SqlState>,
    conn_err: Option<&str>,
    is_closed: bool,
    target: &str,
) -> Option<String> {
    match code {
        Some(&SqlState::QUERY_CANCELED) => Some(
            "the crt.sh guest database cancelled the query (statement timeout); \
             try a more specific search term or a lower --limit"
                .to_string(),
        ),
        Some(&SqlState::TOO_MANY_CONNECTIONS) => Some(
            "the crt.sh guest database is at its connection limit; \
             wait a moment and retry"
                .to_string(),
        ),
        Some(&SqlState::ADMIN_SHUTDOWN)
        | Some(&SqlState::CRASH_SHUTDOWN)
        | Some(&SqlState::CONNECTION_FAILURE)
        | Some(&SqlState::CONNECTION_DOES_NOT_EXIST) => Some(format!(
            "{target} closed the connection mid-query; retry in a moment"
        )),
        // No SQLSTATE: the server never answered. If the connection task
        // recorded why the socket died, that is the real cause.
        None => match conn_err {
            Some(cause) => Some(format!(
                "lost the connection to {target} mid-query ({cause}); the guest database \
                 is shared and drops connections under load — retry in a moment"
            )),
            None if is_closed => Some(format!(
                "the connection to {target} was closed mid-query; retry in a moment"
            )),
            None => None,
        },
        _ => None,
    }
}

fn build_config(conn: &Conn) -> Result<Config> {
    let mut config = match &conn.db_url {
        // The context names where the URL came from. Hardcoding `--db-url` gave
        // a bad `db_url` in the config file a message naming a flag the user
        // never typed, and no path to go and edit.
        Some((url, source)) => url.parse::<Config>().with_context(|| source.describe())?,
        None => {
            let mut c = Config::new();
            c.host(&conn.host)
                .port(conn.port)
                .dbname(&conn.dbname)
                .user(&conn.user);
            c
        }
    };
    config
        .connect_timeout(TCP_CONNECT_TIMEOUT)
        .application_name("crt-query");
    Ok(config)
}

/// The host a config points at. Derived from the parsed config so that a
/// password embedded in a `db_url` never reaches stderr or a CI log.
///
/// Also what decides whether the advice written about the shared guest
/// database applies at all — see [`connect_advice`].
fn host_of(config: &Config) -> String {
    match config.get_hosts().first() {
        Some(Host::Tcp(h)) => h.clone(),
        #[cfg(unix)]
        Some(Host::Unix(path)) => path.display().to_string(),
        None => "<no host>".to_string(),
    }
}

/// Host and port for user-facing messages.
fn target(config: &Config) -> String {
    let port = config.get_ports().first().copied().unwrap_or(5432);
    format!("{}:{port}", host_of(config))
}

/// Full `source` chain of a connection error. `tokio_postgres::Error` renders
/// as a bare "error connecting to server"; the reason lives one level down.
fn chain(err: &tokio_postgres::Error) -> String {
    let mut out = err.to_string();
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        let _ = write!(out, ": {cause}");
        source = cause.source();
    }
    out
}

/// Whether a failed connection attempt is worth retrying.
///
/// Retrying is for load and transport: the guest database drops connections
/// when busy, and that is what `CONNECT_ATTEMPTS` exists for. A rejected
/// password or a database that does not exist will be rejected the same way
/// five times, so retrying only spends the budget and delays the error the
/// caller has to read. That delay is the whole cost now that the attempts are
/// silent; it used to be the delay plus two misleading "retrying..." lines.
fn worth_retrying(err: &tokio_postgres::Error) -> bool {
    worth_retrying_parts(err.code(), &err.to_string())
}

/// The decision, split from the `tokio_postgres::Error` that carries it.
///
/// `tokio_postgres::Error` has no public constructor that carries a SQLSTATE,
/// so a test cannot build the input this rule consumes. Taking the two fields
/// the rule actually reads makes it reachable — and the old test could only
/// assert that `INVALID_PASSWORD.code()` starts with "28", a property of the
/// PostgreSQL spec rather than of anything decided here.
fn worth_retrying_parts(code: Option<&SqlState>, rendered: &str) -> bool {
    match code {
        // Class 28 — invalid authorization specification.
        Some(code) if code.code().starts_with("28") => false,
        Some(&SqlState::INVALID_CATALOG_NAME) => false,
        Some(_) => true,
        // No SQLSTATE means the server never answered — usually load, which is
        // what the retries are for. But a connection setting that is wrong on
        // this side never reaches a server at all, and is decided identically
        // every time: `postgresql:///certwatch` with no host, mismatched
        // host/port lists, a missing password. tokio-postgres gives those no
        // code either, so the rendered text is the only thing separating them,
        // and without this they burned the full retry budget on a verdict that
        // was in from the first attempt — exactly what this function's doc
        // comment above says it exists to prevent.
        None => !is_client_config_error(rendered),
    }
}

/// Whether an error with no SQLSTATE is the caller's configuration rather than
/// the network. These are the two shapes `tokio-postgres` renders for it.
fn is_client_config_error(rendered: &str) -> bool {
    rendered.starts_with("invalid configuration")
        || rendered.starts_with("invalid connection string")
}

/// Why the connect loop stopped, which is what decides the closing advice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ending {
    /// The attempts ran out, or the wall-clock budget did.
    Exhausted,
    /// A deterministic failure; further attempts were pointless.
    Fatal,
}

/// Closing context for a run of failed connection attempts.
///
/// The overload line is for the case it names — attempts spent against the
/// shared guest database — and for nothing else. It used to be attached
/// unconditionally, including on the `Fatal` break, so a rejected password
/// against a host the caller chose themselves was answered with "the crt.sh
/// guest database is shared and may be overloaded": advice they cannot act on,
/// printed in front of the SQLSTATE that named the real problem.
///
/// `attempts` is how many were actually made, not `CONNECT_ATTEMPTS`: the
/// wall-clock budget can stop the loop early, and "after 5 attempts" would then
/// be a count nobody made.
fn connect_advice(target: &str, host: &str, ending: Ending, attempts: u32) -> String {
    let plural = if attempts == 1 { "attempt" } else { "attempts" };
    match ending {
        Ending::Fatal => format!("could not connect to {target}"),
        Ending::Exhausted if host == crate::config::DEFAULT_HOST => format!(
            "could not connect to {target} after {attempts} {plural}; the crt.sh \
             guest database is shared and may be overloaded — wait a moment and retry"
        ),
        Ending::Exhausted => {
            format!("could not connect to {target} after {attempts} {plural}")
        }
    }
}

/// Distinct earlier causes, or `None` when every attempt failed the same way.
///
/// The per-attempt lines this used to print are gone, so the closing error is
/// the only place a swallowed cause can still surface. anyhow prints the last
/// error as the source, so repeating that one would say the same thing twice —
/// and five identical `max_client_conn` rejections, the ordinary case, have
/// nothing to add. Attempts that failed *differently* are a different problem
/// from attempts that failed identically, and this is the surviving record
/// of it.
fn earlier_causes(causes: &[String]) -> Option<String> {
    let (last, earlier) = causes.split_last()?;
    let mut distinct: Vec<&str> = Vec::new();
    for cause in earlier {
        if cause != last && !distinct.contains(&cause.as_str()) {
            distinct.push(cause);
        }
    }
    if distinct.is_empty() {
        return None;
    }
    Some(format!(
        "earlier attempts also failed: {}",
        distinct.join("; ")
    ))
}

/// The wait after attempt `attempt`, before the next one.
///
/// Doubling from [`FIRST_RETRY_DELAY`] to [`MAX_RETRY_DELAY`]: 250ms, 500ms,
/// 1s, 2s. The old flat two seconds was tuned for nothing in particular and was
/// mostly dead time — pgbouncer refuses a client slot instantly and frees one
/// again in well under a second, so the first retry is worth taking almost
/// immediately.
fn backoff(attempt: u32) -> Duration {
    // The shift is clamped before it is applied: `1u32 << 32` panics in debug
    // and wraps to 1 in release, and `attempt` is a loop counter bounded by a
    // constant somebody may well raise.
    let doublings = attempt.saturating_sub(1).min(16);
    FIRST_RETRY_DELAY
        .saturating_mul(1u32 << doublings)
        .min(MAX_RETRY_DELAY)
}

/// Spread a delay out by up to a quarter, using the caller's `nanos`.
///
/// Only ever adds: the backoff above is a floor, not a target. The README
/// advertises `expiring --csv` on a schedule, and cron fires every client on
/// the same second — without this they would all come back on the same
/// 250ms/500ms/1s grid, retrying into each other's contention.
fn jittered(base: Duration, nanos: u32) -> Duration {
    const NANOS_MAX: u64 = 999_999_999;
    let step = u64::from(nanos).min(NANOS_MAX);
    // `base` is capped at MAX_RETRY_DELAY, so a quarter of it is at most 5e8ns
    // and the product below stays four orders of magnitude inside u64.
    let quarter = (base.as_nanos() as u64) / 4;
    base + Duration::from_nanos(quarter * step / NANOS_MAX)
}

/// A jitter source that costs no dependency: the sub-second part of the wall
/// clock. `rand` would be a new crate in a tree audited on every PR, for four
/// delays that need spreading rather than sampling.
fn jitter_nanos() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since_epoch| since_epoch.subsec_nanos())
}

/// Sleep before the next attempt, or report that there will not be one.
///
/// `false` means the budget is spent — either the attempts or the wall clock —
/// so the caller stops rather than sleeping for a retry it will never make.
async fn wait_before_retry(attempt: u32, started: Instant) -> bool {
    if attempt >= CONNECT_ATTEMPTS {
        return false;
    }
    let delay = jittered(backoff(attempt), jitter_nanos());
    if started.elapsed() + delay >= CONNECT_BUDGET {
        return false;
    }
    tokio::time::sleep(delay).await;
    true
}

/// Dial the database, retrying transient failures.
///
/// A failed attempt is not announced as it happens. The failure this retries
/// most often is the shared guest database refusing a client slot, which the
/// next attempt usually gets — and narrating it made a run that then succeeded
/// read like a broken tool. The causes are collected instead, and reported
/// together if no attempt ever connects.
pub async fn connect(conn: &Conn) -> Result<Db> {
    let config = build_config(conn)?;
    let target = target(&config);
    let host = host_of(&config);
    let started = Instant::now();
    let mut causes: Vec<String> = Vec::new();
    let mut last_err: Option<anyhow::Error> = None;
    let mut ending = Ending::Exhausted;
    let mut made = 0;
    for attempt in 1..=CONNECT_ATTEMPTS {
        made = attempt;
        // NoTls: the guest DB serves public read-only data with passwordless
        // auth. Switch to tokio-postgres-rustls if transport privacy is needed.
        //
        // Wrapped in a timeout because `connect_timeout` in the config bounds
        // only the TCP connect: a host that accepts the socket and never
        // completes the startup exchange would otherwise hang here forever.
        let attempted = match tokio::time::timeout(CONNECT_TIMEOUT, config.connect(NoTls)).await {
            Ok(result) => result,
            Err(_) => {
                // Not "connected, but the startup exchange never completed":
                // this bound also spans name resolution and every TCP connect
                // the host resolves to, so naming one phase asserts something
                // the timeout cannot distinguish.
                let stalled = anyhow::anyhow!(
                    "no response from {target} within {}s (name resolution, connect \
                     or the startup exchange did not complete)",
                    CONNECT_TIMEOUT.as_secs()
                );
                causes.push(stalled.to_string());
                last_err = Some(stalled);
                if !wait_before_retry(attempt, started).await {
                    break;
                }
                continue;
            }
        };
        match attempted {
            Ok((client, connection)) => {
                let conn_err: Arc<OnceLock<String>> = Arc::new(OnceLock::new());
                let slot = Arc::clone(&conn_err);
                tokio::spawn(async move {
                    if let Err(e) = connection.await {
                        let _ = slot.set(chain(&e));
                    }
                });
                return Ok(Db {
                    client,
                    conn_err,
                    target,
                });
            }
            Err(e) => {
                let retryable = worth_retrying(&e);
                causes.push(chain(&e));
                last_err = Some(anyhow::Error::new(e));
                if !retryable {
                    ending = Ending::Fatal;
                    break;
                }
                if !wait_before_retry(attempt, started).await {
                    break;
                }
            }
        }
    }
    let advice = connect_advice(&target, &host, ending, made);
    let context = match earlier_causes(&causes) {
        Some(earlier) => format!("{advice} ({earlier})"),
        None => advice,
    };
    Err(last_err.expect("at least one attempt")).context(context)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DbUrlSource;

    fn conn(db_url: Option<&str>) -> Conn {
        Conn {
            host: "crt.sh".to_string(),
            port: 5432,
            dbname: "certwatch".to_string(),
            user: "guest".to_string(),
            db_url: db_url.map(|u| (u.to_string(), DbUrlSource::Flag)),
        }
    }

    #[test]
    fn target_is_host_and_port_for_the_default_config() {
        let config = build_config(&conn(None)).unwrap();
        assert_eq!(target(&config), "crt.sh:5432");
    }

    #[test]
    fn target_never_echoes_a_db_url_password() {
        let config = build_config(&conn(Some(
            "postgresql://me:hunter2@db.internal:6432/certwatch",
        )))
        .unwrap();
        let shown = target(&config);
        assert_eq!(shown, "db.internal:6432");
        assert!(!shown.contains("hunter2"), "password leaked into {shown}");
    }

    #[test]
    fn invalid_db_url_is_rejected_before_connecting() {
        assert!(build_config(&conn(Some("not a url"))).is_err());
    }

    #[test]
    fn the_query_deadline_sits_above_the_servers_own_statement_timeout() {
        // crt.sh cancels at roughly 120s and says so in a way that names the
        // fix. If this bound dropped below that, every too-broad query would
        // surface as our generic "did not answer" instead of the server's
        // actionable QUERY_CANCELED.
        assert!(
            QUERY_TIMEOUT > Duration::from_secs(120),
            "QUERY_TIMEOUT {QUERY_TIMEOUT:?} would pre-empt the server's own timeout"
        );
        // And a connection attempt must not be able to outlast the whole
        // retry budget it is one fifth of.
        assert!(CONNECT_TIMEOUT < QUERY_TIMEOUT);
        // The connect phase, retries and waits included, plus the one attempt
        // that can still be in flight when the budget runs out. If this could
        // exceed the statement cap, a wedged connect would outlast the thing it
        // exists to protect a scheduled run from.
        let worst_case = CONNECT_BUDGET + CONNECT_TIMEOUT;
        assert!(worst_case < QUERY_TIMEOUT);
        // And it must stay under what the schedule this replaced could spend.
        // Nothing is printed during the phase any more, so every second of it
        // is silence a caller cannot tell from a hang — this is the one bound
        // that got stricter when the per-attempt lines went away.
        const PREVIOUS_WORST_CASE: Duration = Duration::from_secs(49);
        assert!(
            worst_case < PREVIOUS_WORST_CASE,
            "a silent connect phase ({worst_case:?}) may not outlast the narrated \
             one it replaced ({PREVIOUS_WORST_CASE:?})"
        );
        // The per-address TCP bound sits under the whole-attempt bound. Note
        // this ordering alone does not bound an attempt: tokio-postgres applies
        // TCP_CONNECT_TIMEOUT once per resolved address, so CONNECT_TIMEOUT is
        // what actually caps the total.
        assert!(TCP_CONNECT_TIMEOUT < CONNECT_TIMEOUT);
    }

    #[test]
    fn a_rejected_credential_is_not_retried() {
        // Retrying is for load and transport. A password the server rejected
        // will be rejected identically four more times, so retrying only makes
        // the caller wait out a verdict that was already in.
        //
        // This calls the decision function. The version that asserted
        // `INVALID_PASSWORD.code().starts_with("28")` pinned the PostgreSQL
        // spec, not this rule: flipping the prefix here to "29" left it green.
        for code in [
            SqlState::INVALID_PASSWORD,
            SqlState::INVALID_AUTHORIZATION_SPECIFICATION,
            SqlState::INVALID_CATALOG_NAME,
        ] {
            assert!(
                !worth_retrying_parts(Some(&code), "db error"),
                "{code:?} is deterministic and must not be retried"
            );
        }
    }

    #[test]
    fn the_authorization_class_prefix_still_means_what_it_did() {
        // A canary for the `starts_with("28")` above, which is the whole of
        // what separates a rejected password from three attempts.
        for code in [
            SqlState::INVALID_PASSWORD,
            SqlState::INVALID_AUTHORIZATION_SPECIFICATION,
        ] {
            assert!(
                code.code().starts_with("28"),
                "{code:?} is no longer class 28; worth_retrying_parts needs updating"
            );
        }
        assert_eq!(SqlState::INVALID_CATALOG_NAME.code(), "3D000");
    }

    #[test]
    fn load_and_transport_failures_are_retried() {
        assert!(worth_retrying_parts(
            Some(&SqlState::TOO_MANY_CONNECTIONS),
            "db error"
        ));
        assert!(worth_retrying_parts(
            Some(&SqlState::CONNECTION_FAILURE),
            "db error"
        ));
        assert!(worth_retrying_parts(
            None,
            "error connecting to server: Connection refused (os error 61)"
        ));
    }

    #[test]
    fn a_client_side_configuration_error_is_not_retried() {
        // No SQLSTATE, because no server ever saw these: a URL with no host,
        // mismatched host/port lists, a missing password. They fall in the
        // same `code() == None` bucket as a dropped socket, and without this
        // they would burn the whole retry budget on a settled verdict.
        // The rendered prefix is the only thing telling them apart — both
        // spellings verified against tokio-postgres 0.7.18.
        for rendered in ["invalid configuration", "invalid connection string"] {
            assert!(
                !worth_retrying_parts(None, rendered),
                "{rendered:?} cannot succeed on a second attempt"
            );
        }
    }

    #[test]
    fn each_mapped_sqlstate_explains_itself() {
        let cancelled =
            explain_context(Some(&SqlState::QUERY_CANCELED), None, false, "crt.sh:5432").unwrap();
        assert!(cancelled.contains("statement timeout"), "{cancelled}");
        assert!(cancelled.contains("--limit"), "{cancelled}");

        let busy = explain_context(
            Some(&SqlState::TOO_MANY_CONNECTIONS),
            None,
            false,
            "crt.sh:5432",
        )
        .unwrap();
        assert!(busy.contains("connection limit"), "{busy}");

        let shutdown = explain_context(
            Some(&SqlState::ADMIN_SHUTDOWN),
            None,
            false,
            "db.internal:6432",
        )
        .unwrap();
        assert!(shutdown.contains("db.internal:6432"), "{shutdown}");
        assert!(shutdown.contains("closed the connection"), "{shutdown}");
    }

    #[test]
    fn an_unmapped_failure_leaves_the_raw_error_to_speak_for_itself() {
        // Wrapping an unrecognised SQLSTATE in a guess would bury the one
        // description that is actually authoritative.
        assert_eq!(
            explain_context(Some(&SqlState::SYNTAX_ERROR), None, false, "crt.sh:5432"),
            None
        );
        assert_eq!(explain_context(None, None, false, "crt.sh:5432"), None);
    }

    #[test]
    fn the_connection_tasks_cause_beats_a_bare_closed_flag() {
        let with_cause =
            explain_context(None, Some("connection reset by peer"), true, "crt.sh:5432").unwrap();
        assert!(
            with_cause.contains("connection reset by peer"),
            "the socket's real cause was dropped: {with_cause}"
        );

        let bare = explain_context(None, None, true, "crt.sh:5432").unwrap();
        assert!(bare.contains("was closed mid-query"), "{bare}");
    }

    #[test]
    fn overload_advice_is_only_offered_where_it_could_be_true() {
        let guest = connect_advice("crt.sh:5432", "crt.sh", Ending::Exhausted, CONNECT_ATTEMPTS);
        assert!(guest.contains("guest database is shared"), "{guest}");

        // A host the caller pointed us at themselves. Telling them to wait out
        // load on crt.sh is advice about a service they are not talking to.
        let own = connect_advice(
            "db.internal:6432",
            "db.internal",
            Ending::Exhausted,
            CONNECT_ATTEMPTS,
        );
        assert!(!own.contains("guest database"), "{own}");
        assert!(own.contains("db.internal:6432"), "{own}");

        // A rejected password stopped after one attempt; it was never load,
        // and the SQLSTATE behind this context is what names the real problem.
        let fatal = connect_advice("crt.sh:5432", "crt.sh", Ending::Fatal, 1);
        assert!(!fatal.contains("overloaded"), "{fatal}");
        assert!(!fatal.contains("attempts"), "{fatal}");
    }

    #[test]
    fn the_advice_counts_the_attempts_that_were_actually_made() {
        // The wall-clock budget can stop the loop before the attempts run out,
        // so this cannot go back to reading CONNECT_ATTEMPTS: it would report a
        // count nobody made. Three stalled attempts is the realistic shape.
        let short = connect_advice("crt.sh:5432", "crt.sh", Ending::Exhausted, 3);
        assert!(short.contains("after 3 attempts"), "{short}");
        assert!(
            !short.contains(&format!("after {CONNECT_ATTEMPTS} attempts")),
            "reported the budget rather than the attempts made: {short}"
        );
        let one = connect_advice("crt.sh:5432", "crt.sh", Ending::Exhausted, 1);
        assert!(one.contains("after 1 attempt;"), "{one}");
    }

    #[test]
    fn the_backoff_starts_short_stays_capped_and_never_goes_backwards() {
        assert_eq!(backoff(1), FIRST_RETRY_DELAY);
        let mut previous = Duration::ZERO;
        for attempt in 1..=CONNECT_ATTEMPTS {
            let delay = backoff(attempt);
            assert!(delay >= previous, "backoff({attempt}) went backwards");
            assert!(
                delay <= MAX_RETRY_DELAY,
                "backoff({attempt}) exceeds the cap"
            );
            previous = delay;
        }
        // The shift is clamped, not trusted: raising CONNECT_ATTEMPTS past 32
        // would otherwise be a panic in debug and a wrapped delay in release.
        assert_eq!(backoff(u32::MAX), MAX_RETRY_DELAY);
        assert_eq!(backoff(0), FIRST_RETRY_DELAY);
    }

    #[test]
    fn jitter_only_ever_adds_and_stays_inside_a_quarter() {
        // A jitter that could subtract would retry sooner than the backoff
        // says, which is the opposite of what it is for.
        for base in [FIRST_RETRY_DELAY, MAX_RETRY_DELAY] {
            for nanos in [0, 500_000_000, 999_999_999, u32::MAX] {
                let delay = jittered(base, nanos);
                assert!(
                    delay >= base,
                    "jittered({base:?}, {nanos}) shortened the wait"
                );
                assert!(
                    delay <= base + base / 4,
                    "jittered({base:?}, {nanos}) added more than a quarter"
                );
            }
        }
        assert_eq!(jittered(MAX_RETRY_DELAY, 0), MAX_RETRY_DELAY);
    }

    #[test]
    fn the_whole_retry_schedule_fits_inside_the_wall_clock_budget() {
        // Otherwise the budget, not the attempt count, would silently decide
        // how many attempts a fast-failing host gets — and a busy crt.sh, which
        // refuses instantly, is exactly that host.
        let waited: Duration = (1..CONNECT_ATTEMPTS)
            .map(|attempt| jittered(backoff(attempt), 999_999_999))
            .sum();
        assert!(
            waited < CONNECT_BUDGET,
            "the retry waits alone ({waited:?}) spend the {CONNECT_BUDGET:?} budget"
        );
    }

    #[test]
    fn identical_failures_are_reported_once() {
        // The ordinary case: five max_client_conn rejections in a row. anyhow
        // prints the last as the source, so there is nothing to add.
        let same = "db error: ERROR: no more connections allowed (max_client_conn)".to_string();
        assert_eq!(earlier_causes(&vec![same; 5]), None);
        assert_eq!(earlier_causes(&[]), None);
    }

    #[test]
    fn causes_that_differ_survive_the_silence() {
        // Nothing prints an attempt as it fails any more, so a run that failed
        // three different ways has only this to say so.
        let causes = [
            "no response from crt.sh:5432 within 15s".to_string(),
            "db error: ERROR: no more connections allowed (max_client_conn)".to_string(),
            "error connecting to server: Connection refused".to_string(),
        ];
        let earlier = earlier_causes(&causes).unwrap();
        assert!(earlier.contains("no response"), "{earlier}");
        assert!(earlier.contains("max_client_conn"), "{earlier}");
        // The last cause is the error anyhow prints as the source; repeating it
        // here would say the same thing twice in one line.
        assert!(!earlier.contains("Connection refused"), "{earlier}");

        // And a cause seen twice is still listed once.
        let repeated = [
            "stalled".to_string(),
            "stalled".to_string(),
            "refused".to_string(),
        ];
        assert_eq!(
            earlier_causes(&repeated).unwrap(),
            "earlier attempts also failed: stalled"
        );
    }

    #[test]
    fn a_config_file_db_url_names_the_file_and_not_a_flag() {
        let mut c = conn(Some("not a url"));
        c.db_url = Some(("not a url".to_string(), DbUrlSource::ConfigFile));
        let err = format!("{:#}", build_config(&c).unwrap_err());
        assert!(err.contains("db_url"), "{err}");
        assert!(
            !err.contains("--db-url"),
            "blamed a flag the user never typed: {err}"
        );
    }
}
