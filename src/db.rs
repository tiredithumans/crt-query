use std::fmt::Write as _;
use std::io::IsTerminal;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio_postgres::config::Host;
use tokio_postgres::error::SqlState;
use tokio_postgres::types::{ToSql, Type};
use tokio_postgres::{Client, Config, NoTls, Row};

use crate::config::Conn;

const CONNECT_ATTEMPTS: u32 = 3;
const RETRY_DELAY: Duration = Duration::from_secs(2);

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
/// three times, so retrying only delays the error the caller has to read —
/// with two misleading "retrying..." lines in front of it.
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
        // three times: `postgresql:///certwatch` with no host, mismatched
        // host/port lists, a missing password. tokio-postgres gives those no
        // code either, so the rendered text is the only thing separating them,
        // and without this they burned the full retry budget behind two
        // misleading "retrying..." lines — exactly what this function's doc
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
    /// Every attempt was spent.
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
fn connect_advice(target: &str, host: &str, ending: Ending) -> String {
    match ending {
        Ending::Fatal => format!("could not connect to {target}"),
        Ending::Exhausted if host == crate::config::DEFAULT_HOST => format!(
            "could not connect to {target} after {CONNECT_ATTEMPTS} attempts; the crt.sh \
             guest database is shared and may be overloaded — wait a moment and retry"
        ),
        Ending::Exhausted => {
            format!("could not connect to {target} after {CONNECT_ATTEMPTS} attempts")
        }
    }
}

pub async fn connect(conn: &Conn) -> Result<Db> {
    let config = build_config(conn)?;
    let target = target(&config);
    let host = host_of(&config);
    let mut last_err: Option<anyhow::Error> = None;
    let mut ending = Ending::Exhausted;
    for attempt in 1..=CONNECT_ATTEMPTS {
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
                if attempt < CONNECT_ATTEMPTS {
                    eprintln!(
                        "connection attempt {attempt}/{CONNECT_ATTEMPTS} to {target} \
                         failed: {stalled}; retrying..."
                    );
                    tokio::time::sleep(RETRY_DELAY).await;
                }
                last_err = Some(stalled);
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
                if attempt < CONNECT_ATTEMPTS && retryable {
                    eprintln!(
                        "connection attempt {attempt}/{CONNECT_ATTEMPTS} to {target} failed: {}; retrying...",
                        chain(&e)
                    );
                    tokio::time::sleep(RETRY_DELAY).await;
                }
                let fatal = !retryable;
                last_err = Some(anyhow::Error::new(e));
                if fatal {
                    ending = Ending::Fatal;
                    break;
                }
            }
        }
    }
    Err(last_err.expect("at least one attempt")).context(connect_advice(&target, &host, ending))
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
        // retry budget it is one third of.
        assert!(CONNECT_TIMEOUT < QUERY_TIMEOUT);
        // The per-address TCP bound sits under the whole-attempt bound. Note
        // this ordering alone does not bound an attempt: tokio-postgres applies
        // TCP_CONNECT_TIMEOUT once per resolved address, so CONNECT_TIMEOUT is
        // what actually caps the total.
        assert!(TCP_CONNECT_TIMEOUT < CONNECT_TIMEOUT);
    }

    #[test]
    fn a_rejected_credential_is_not_retried() {
        // Retrying is for load and transport. A password the server rejected
        // will be rejected identically twice more, so retrying only buries the
        // real error under two misleading "retrying..." lines.
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
        // same `code() == None` bucket as a dropped socket and used to burn
        // the whole retry budget behind two misleading "retrying..." lines.
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
        let guest = connect_advice("crt.sh:5432", "crt.sh", Ending::Exhausted);
        assert!(guest.contains("guest database is shared"), "{guest}");

        // A host the caller pointed us at themselves. Telling them to wait out
        // load on crt.sh is advice about a service they are not talking to.
        let own = connect_advice("db.internal:6432", "db.internal", Ending::Exhausted);
        assert!(!own.contains("guest database"), "{own}");
        assert!(own.contains("db.internal:6432"), "{own}");

        // A rejected password stopped after one attempt; it was never load,
        // and the SQLSTATE behind this context is what names the real problem.
        let fatal = connect_advice("crt.sh:5432", "crt.sh", Ending::Fatal);
        assert!(!fatal.contains("overloaded"), "{fatal}");
        assert!(!fatal.contains("attempts"), "{fatal}");
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
