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
        self.client
            .query_typed(sql, params)
            .await
            .map_err(|e| self.explain(e))
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
        // A SQLSTATE the server actually sent is authoritative and more
        // specific than anything the connection task can say, so it is checked
        // first; the dead-connection cause explains the no-SQLSTATE case,
        // where the server never answered at all.
        let context = match err.code() {
            Some(&SqlState::QUERY_CANCELED) => {
                "the crt.sh guest database cancelled the query (statement timeout); \
                 try a more specific search term or a lower --limit"
                    .to_string()
            }
            Some(&SqlState::TOO_MANY_CONNECTIONS) => {
                "the crt.sh guest database is at its connection limit; \
                 wait a moment and retry"
                    .to_string()
            }
            Some(&SqlState::ADMIN_SHUTDOWN)
            | Some(&SqlState::CRASH_SHUTDOWN)
            | Some(&SqlState::CONNECTION_FAILURE)
            | Some(&SqlState::CONNECTION_DOES_NOT_EXIST) => {
                format!(
                    "{} closed the connection mid-query; retry in a moment",
                    self.target
                )
            }
            // No SQLSTATE: the server never answered. If the connection task
            // recorded why the socket died, that is the real cause.
            None => match self.conn_err.get() {
                Some(cause) => format!(
                    "lost the connection to {} mid-query ({cause}); the guest database \
                     is shared and drops connections under load — retry in a moment",
                    self.target
                ),
                None if err.is_closed() => format!(
                    "the connection to {} was closed mid-query; retry in a moment",
                    self.target
                ),
                None => return anyhow::Error::new(err),
            },
            _ => return anyhow::Error::new(err),
        };
        anyhow::Error::new(err).context(context)
    }
}

fn build_config(conn: &Conn) -> Result<Config> {
    let mut config = match &conn.db_url {
        Some(url) => url.parse::<Config>().context("invalid --db-url")?,
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
        .connect_timeout(Duration::from_secs(10))
        .application_name("crt-query");
    Ok(config)
}

/// Host and port for user-facing messages. Derived from the parsed config so
/// that a password embedded in `--db-url` never reaches stderr or a CI log.
fn target(config: &Config) -> String {
    let host = match config.get_hosts().first() {
        Some(Host::Tcp(h)) => h.clone(),
        #[cfg(unix)]
        Some(Host::Unix(path)) => path.display().to_string(),
        None => "<no host>".to_string(),
    };
    let port = config.get_ports().first().copied().unwrap_or(5432);
    format!("{host}:{port}")
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

pub async fn connect(conn: &Conn) -> Result<Db> {
    let config = build_config(conn)?;
    let target = target(&config);
    let mut last_err = None;
    for attempt in 1..=CONNECT_ATTEMPTS {
        // NoTls: the guest DB serves public read-only data with passwordless
        // auth. Switch to tokio-postgres-rustls if transport privacy is needed.
        match config.connect(NoTls).await {
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
                if attempt < CONNECT_ATTEMPTS {
                    eprintln!(
                        "connection attempt {attempt}/{CONNECT_ATTEMPTS} to {target} failed: {}; retrying...",
                        chain(&e)
                    );
                    tokio::time::sleep(RETRY_DELAY).await;
                }
                last_err = Some(e);
            }
        }
    }
    Err(anyhow::Error::new(last_err.expect("at least one attempt"))).with_context(|| {
        format!(
            "could not connect to {target} after {CONNECT_ATTEMPTS} attempts; the crt.sh \
             guest database is shared and may be overloaded — wait a moment and retry"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(db_url: Option<&str>) -> Conn {
        Conn {
            host: "crt.sh".to_string(),
            port: 5432,
            dbname: "certwatch".to_string(),
            user: "guest".to_string(),
            db_url: db_url.map(str::to_string),
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
}
