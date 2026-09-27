use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::cli::ConnOpts;

/// Built-in connection defaults, used when neither a CLI flag nor the config
/// file provides a value.
pub const DEFAULT_HOST: &str = "crt.sh";
pub const DEFAULT_PORT: u16 = 5432;
pub const DEFAULT_DBNAME: &str = "certwatch";
pub const DEFAULT_USER: &str = "guest";

/// Connection settings read from the config file. Every field is optional;
/// absent fields fall back to the built-in defaults, and a CLI flag or a
/// `CRT_QUERY_*` environment variable overrides any of them.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub dbname: Option<String>,
    pub user: Option<String>,
    pub db_url: Option<String>,
    /// Whether to use the local result cache at all. `--no-cache` still wins.
    pub cache: Option<bool>,
    /// How long a cached `search`/`expiring` result, or a `cert` lookup that
    /// found nothing, stays usable, in seconds. A found certificate keeps its
    /// own much longer lifetime — the record it caches cannot change.
    pub cache_ttl_secs: Option<u64>,
}

/// Where a resolved `db_url` came from.
///
/// Carried so that an unparseable one can name the thing the caller actually
/// set. The error context was hardcoded to `--db-url`, so a bad `db_url` in the
/// config file produced a message byte-identical to the flag case — naming a
/// flag the user never typed, and no file to go and look at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbUrlSource {
    Flag,
    /// `CRT_QUERY_DB_URL`. Named apart from `Flag` for the same reason
    /// `ConfigFile` is: blaming `--db-url` for a variable sends the reader to
    /// a command line that holds nothing wrong.
    Env,
    ConfigFile,
}

impl DbUrlSource {
    /// How to describe a `db_url` that failed to parse.
    pub fn describe(self) -> String {
        match self {
            Self::Flag => "invalid --db-url".to_string(),
            Self::Env => format!("invalid {ENV_DB_URL}"),
            Self::ConfigFile => match config_path() {
                Some(path) => format!("invalid `db_url` in {}", path.display()),
                None => "invalid `db_url` in the config file".to_string(),
            },
        }
    }
}

/// The environment variables that can stand in for the connection flags.
pub const ENV_HOST: &str = "CRT_QUERY_HOST";
pub const ENV_PORT: &str = "CRT_QUERY_PORT";
pub const ENV_DBNAME: &str = "CRT_QUERY_DBNAME";
pub const ENV_USER: &str = "CRT_QUERY_USER";
pub const ENV_DB_URL: &str = "CRT_QUERY_DB_URL";

/// Connection settings read from `CRT_QUERY_*` environment variables.
///
/// They sit between the command line and the config file: a container or a CI
/// job can point the tool somewhere without writing a file, and an explicit
/// flag still wins.
///
/// Read here rather than through clap's `env` attribute, which was tried and
/// had two faults. clap parses every global argument for every subcommand, so
/// an exported-but-empty `CRT_QUERY_PORT` made even `completions` exit 2; and
/// it treats an empty variable as a value, where `export CRT_QUERY_HOST=` is
/// how a shell user says "unset". Reading them only when a connection is
/// resolved, with empty meaning absent, avoids both — and knowing the source
/// directly is what lets a bad `CRT_QUERY_DB_URL` be reported under its own
/// name.
#[derive(Debug, Default)]
pub struct EnvConfig {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub dbname: Option<String>,
    pub user: Option<String>,
    pub db_url: Option<String>,
}

impl EnvConfig {
    /// Read the variables from the process environment.
    pub fn load() -> Result<Self> {
        Self::from_lookup(|name| std::env::var_os(name))
    }

    /// Read the variables through `lookup`, so tests need not touch the
    /// process environment (`std::env::set_var` is `unsafe` under edition
    /// 2024, and racy across parallel tests besides).
    fn from_lookup(lookup: impl Fn(&str) -> Option<OsString>) -> Result<Self> {
        let text = |name: &str| -> Result<Option<String>> {
            match lookup(name) {
                None => Ok(None),
                Some(value) if value.is_empty() => Ok(None),
                Some(value) => value
                    .into_string()
                    .map(Some)
                    .map_err(|_| anyhow::anyhow!("{name} is not valid UTF-8")),
            }
        };
        let port = match text(ENV_PORT)? {
            None => None,
            Some(port) => Some(port.trim().parse::<u16>().with_context(|| {
                format!("invalid {ENV_PORT} {port:?}: expected a port number from 0 to 65535")
            })?),
        };
        Ok(Self {
            host: text(ENV_HOST)?,
            port,
            dbname: text(ENV_DBNAME)?,
            user: text(ENV_USER)?,
            db_url: text(ENV_DB_URL)?,
        })
    }
}

/// Fully resolved connection settings, after CLI flags and the config file
/// have been folded into the built-in defaults.
#[derive(Debug, Clone)]
pub struct Conn {
    pub host: String,
    pub port: u16,
    pub dbname: String,
    pub user: String,
    /// The URL and where it was set, so a parse failure can name its source.
    pub db_url: Option<(String, DbUrlSource)>,
}

/// Where the config file is read from, if a location can be determined at all.
pub fn config_path() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        Some(config_path_in(&config_root(std::env::var_os("APPDATA"))?))
    }
    #[cfg(not(windows))]
    {
        Some(config_path_in(&config_root(
            std::env::var_os("XDG_CONFIG_HOME"),
            std::env::var_os("HOME"),
        )?))
    }
}

/// The directory the config file is looked up under, or `None` if the
/// environment does not name one absolutely.
///
/// Absolute is the whole point. The XDG spec requires a relative
/// `$XDG_CONFIG_HOME` to be ignored, and a relative or empty `$HOME` is no
/// better: either resolves the config file against the process's current
/// directory, so running inside a tree that happens to carry
/// `./.config/crt-query/config.toml` would silently redirect every query —
/// `db_url` included — to a host the caller never chose. The only clue would be
/// the stderr hint, which is suppressed when stderr is not a terminal, so the
/// environments where this is easiest to hit (cron, systemd units, containers,
/// `env -i`) are exactly the ones that would never show it.
///
/// Taking the values as arguments rather than reading the environment keeps
/// this testable: `std::env::set_var` is `unsafe` under edition 2024.
#[cfg(not(windows))]
fn config_root(xdg: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    // `is_absolute` subsumes the emptiness check: "" is not an absolute path.
    xdg.map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            home.map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .map(|h| h.join(".config"))
        })
}

/// Windows counterpart: `%APPDATA%` is subject to the same reasoning.
#[cfg(windows)]
fn config_root(appdata: Option<OsString>) -> Option<PathBuf> {
    appdata.map(PathBuf::from).filter(|p| p.is_absolute())
}

fn config_path_in(config_root: &Path) -> PathBuf {
    config_root.join("crt-query").join("config.toml")
}

/// Read and parse the config file if one exists. A missing file is not an
/// error — it simply means every setting comes from flags or defaults.
pub fn load() -> Result<FileConfig> {
    let Some(path) = config_path() else {
        return Ok(FileConfig::default());
    };
    if !path.is_file() {
        return Ok(FileConfig::default());
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading config file {}", path.display()))?;
    let cfg: FileConfig =
        toml::from_str(&text).with_context(|| format!("parsing config file {}", path.display()))?;
    Ok(cfg)
}

/// Fold CLI flags, environment variables, the config file and built-in
/// defaults into one connection.
///
/// Precedence, highest first: CLI flag, `CRT_QUERY_*` environment variable,
/// config file, built-in default. A `db_url` from any source overrides the
/// individual host/port/dbname/user settings entirely, mirroring what
/// `--db-url` does on the command line.
pub fn resolve(cli: &ConnOpts, env: &EnvConfig, file: &FileConfig) -> Conn {
    let db_url = cli
        .db_url
        .clone()
        .map(|url| (url, DbUrlSource::Flag))
        .or_else(|| env.db_url.clone().map(|url| (url, DbUrlSource::Env)))
        .or_else(|| {
            file.db_url
                .clone()
                .map(|url| (url, DbUrlSource::ConfigFile))
        });
    let text =
        |flag: &Option<String>, env: &Option<String>, file: &Option<String>, default: &str| {
            flag.clone()
                .or_else(|| env.clone())
                .or_else(|| file.clone())
                .unwrap_or_else(|| default.to_string())
        };
    Conn {
        host: text(&cli.host, &env.host, &file.host, DEFAULT_HOST),
        port: cli.port.or(env.port).or(file.port).unwrap_or(DEFAULT_PORT),
        dbname: text(&cli.dbname, &env.dbname, &file.dbname, DEFAULT_DBNAME),
        user: text(&cli.user, &env.user, &file.user, DEFAULT_USER),
        db_url,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `resolve` with no `CRT_QUERY_*` variables set: the flag-versus-file
    /// cases below predate the environment layer and are about that pair.
    fn without_env(cli: &ConnOpts, file: &FileConfig) -> Conn {
        resolve(cli, &EnvConfig::default(), file)
    }

    #[cfg(not(windows))]
    fn root(xdg: Option<&str>, home: Option<&str>) -> Option<String> {
        config_root(xdg.map(Into::into), home.map(Into::into))
            .map(|p| p.to_string_lossy().into_owned())
    }

    #[test]
    #[cfg(not(windows))]
    fn an_absolute_xdg_config_home_wins() {
        assert_eq!(
            root(Some("/xdg"), Some("/home/u")),
            Some("/xdg".to_string())
        );
    }

    #[test]
    #[cfg(not(windows))]
    fn a_relative_xdg_config_home_is_ignored_as_the_spec_requires() {
        // Not "./.config" resolved against wherever the process started —
        // that would let a directory the caller merely happens to be standing
        // in redirect every query, db_url included.
        assert_eq!(
            root(Some(".config"), Some("/home/u")),
            Some("/home/u/.config".to_string())
        );
    }

    #[test]
    #[cfg(not(windows))]
    fn an_empty_xdg_config_home_falls_through_to_home() {
        assert_eq!(
            root(Some(""), Some("/home/u")),
            Some("/home/u/.config".to_string())
        );
    }

    #[test]
    #[cfg(not(windows))]
    fn a_relative_or_missing_home_yields_no_config_path_at_all() {
        assert_eq!(root(Some(".config"), Some("home/u")), None);
        assert_eq!(root(None, None), None);
        assert_eq!(root(Some(""), Some("")), None);
    }

    #[test]
    #[cfg(windows)]
    fn only_an_absolute_appdata_yields_a_config_path() {
        assert!(config_root(Some(r"C:\\Users\\u\\AppData\\Roaming".into())).is_some());
        assert!(config_root(Some(r"AppData\\Roaming".into())).is_none());
        assert!(config_root(Some("".into())).is_none());
        assert!(config_root(None).is_none());
    }

    fn cli(host: Option<&str>, db_url: Option<&str>) -> ConnOpts {
        ConnOpts {
            host: host.map(str::to_string),
            port: None,
            dbname: None,
            user: None,
            db_url: db_url.map(str::to_string),
        }
    }

    fn file(host: Option<&str>, db_url: Option<&str>) -> FileConfig {
        FileConfig {
            host: host.map(str::to_string),
            db_url: db_url.map(str::to_string),
            ..FileConfig::default()
        }
    }

    /// The URL and its source together: the source is what decides whether a
    /// parse failure names `--db-url` or the config file.
    fn url_of(conn: &Conn) -> Option<(&str, DbUrlSource)> {
        conn.db_url.as_ref().map(|(url, src)| (url.as_str(), *src))
    }

    #[test]
    fn defaults_apply_when_nothing_is_set() {
        let conn = without_env(&cli(None, None), &FileConfig::default());
        assert_eq!(conn.host, DEFAULT_HOST);
        assert_eq!(conn.port, DEFAULT_PORT);
        assert_eq!(conn.dbname, DEFAULT_DBNAME);
        assert_eq!(conn.user, DEFAULT_USER);
        assert!(conn.db_url.is_none());
    }

    #[test]
    fn cli_beats_the_config_file() {
        let conn = without_env(
            &cli(Some("cli.example"), None),
            &file(Some("file.example"), None),
        );
        assert_eq!(conn.host, "cli.example");
    }

    #[test]
    fn config_file_beats_the_defaults() {
        let conn = without_env(&cli(None, None), &file(Some("file.example"), None));
        assert_eq!(conn.host, "file.example");
    }

    #[test]
    fn a_cli_db_url_overrides_everything_else() {
        let conn = without_env(
            &cli(
                Some("ignored.example"),
                Some("postgresql://u@cli.example/db"),
            ),
            &file(Some("also-ignored.example"), None),
        );
        assert_eq!(
            url_of(&conn),
            Some(("postgresql://u@cli.example/db", DbUrlSource::Flag))
        );
    }

    #[test]
    fn a_config_file_db_url_overrides_the_individual_fields() {
        let conn = without_env(
            &cli(Some("ignored.example"), None),
            &file(None, Some("postgresql://u@f.example/db")),
        );
        assert_eq!(
            url_of(&conn),
            Some(("postgresql://u@f.example/db", DbUrlSource::ConfigFile)),
            "a config-file db_url must be marked as such, or a parse failure \
             blames a --db-url flag the user never typed"
        );
    }

    fn env_of(pairs: &[(&str, &str)]) -> Result<EnvConfig> {
        let pairs: Vec<(String, OsString)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), OsString::from(*v)))
            .collect();
        EnvConfig::from_lookup(|name| {
            pairs
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        })
    }

    /// The environment sits between the two: it beats the file and loses to
    /// the flag, field by field.
    #[test]
    fn the_environment_beats_the_file_and_loses_to_the_flag() {
        let env = env_of(&[
            (ENV_HOST, "env.example"),
            (ENV_PORT, "7"),
            (ENV_DBNAME, "env-db"),
            (ENV_USER, "env-user"),
        ])
        .unwrap();
        let file = FileConfig {
            host: Some("file.example".to_string()),
            port: Some(2),
            dbname: Some("file-db".to_string()),
            user: Some("file-user".to_string()),
            ..FileConfig::default()
        };
        let over_file = resolve(&cli(None, None), &env, &file);
        assert_eq!(over_file.host, "env.example");
        assert_eq!(over_file.port, 7);
        assert_eq!(over_file.dbname, "env-db");
        assert_eq!(over_file.user, "env-user");

        let flag = resolve(&cli(Some("cli.example"), None), &env, &file);
        assert_eq!(
            flag.host, "cli.example",
            "an explicit flag must beat the environment"
        );
    }

    /// A URL from `CRT_QUERY_DB_URL` has to be reported as coming from there,
    /// beats the file's, and loses to `--db-url`.
    #[test]
    fn an_environment_db_url_is_named_as_such() {
        let env = env_of(&[(ENV_DB_URL, "postgresql://u@env.example/db")]).unwrap();
        let file = file(None, Some("postgresql://u@f.example/db"));
        assert_eq!(
            url_of(&resolve(&cli(None, None), &env, &file)),
            Some(("postgresql://u@env.example/db", DbUrlSource::Env))
        );
        assert_eq!(
            url_of(&resolve(
                &cli(None, Some("postgresql://u@cli.example/db")),
                &env,
                &file
            )),
            Some(("postgresql://u@cli.example/db", DbUrlSource::Flag))
        );
        assert_eq!(DbUrlSource::Env.describe(), "invalid CRT_QUERY_DB_URL");
    }

    /// `export CRT_QUERY_HOST=` is how a shell user unsets a variable for one
    /// command; treating it as a host named "" would send the connection
    /// nowhere rather than to the next layer down.
    #[test]
    fn an_empty_variable_counts_as_unset() {
        let env = env_of(&[(ENV_HOST, ""), (ENV_PORT, ""), (ENV_DB_URL, "")]).unwrap();
        assert!(env.host.is_none() && env.port.is_none() && env.db_url.is_none());
    }

    #[test]
    fn a_bad_port_variable_is_an_error_that_names_it() {
        for bad in ["abc", "70000", "-1"] {
            let err = format!("{:#}", env_of(&[(ENV_PORT, bad)]).unwrap_err());
            assert!(err.contains(ENV_PORT), "{err}");
        }
        assert_eq!(env_of(&[(ENV_PORT, " 6432 ")]).unwrap().port, Some(6432));
    }

    #[test]
    #[cfg(unix)]
    fn a_non_utf8_variable_is_an_error_that_names_it() {
        use std::os::unix::ffi::OsStringExt;
        let bad = OsString::from_vec(vec![0x66, 0xff]);
        let err = EnvConfig::from_lookup(|name| (name == ENV_HOST).then(|| bad.clone()))
            .unwrap_err()
            .to_string();
        assert!(err.contains(ENV_HOST), "{err}");
    }

    #[test]
    fn a_cli_db_url_beats_a_config_file_db_url() {
        let conn = without_env(
            &cli(None, Some("postgresql://u@cli.example/db")),
            &file(None, Some("postgresql://u@f.example/db")),
        );
        assert_eq!(
            url_of(&conn),
            Some(("postgresql://u@cli.example/db", DbUrlSource::Flag))
        );
    }

    /// `resolve` folds five settings; the helpers above vary only `host` and
    /// `db_url`, hardcoding port/dbname/user to None, so nothing exercised
    /// their precedence. Deleting a fallback is caught by clippy's dead-field
    /// lint, but an *inversion* — `file.port.or(cli.port)` — passes cargo test,
    /// clippy at -D warnings and fmt --check, while letting a stale
    /// config.toml silently override an explicit --port or --user. Against an
    /// internal mirror also on 5432 that is a successful connection to the
    /// wrong target, with db::hint suppressed whenever stderr is not a tty.
    #[test]
    fn port_dbname_and_user_take_the_cli_then_the_file_then_the_default() {
        let from_file = || FileConfig {
            port: Some(2),
            dbname: Some("file-db".to_string()),
            user: Some("file-user".to_string()),
            ..FileConfig::default()
        };
        let from_cli = ConnOpts {
            host: None,
            port: Some(1),
            dbname: Some("cli-db".to_string()),
            user: Some("cli-user".to_string()),
            db_url: None,
        };

        let both = resolve(&from_cli, &EnvConfig::default(), &from_file());
        assert_eq!(both.port, 1, "an explicit --port must beat the config file");
        assert_eq!(both.dbname, "cli-db");
        assert_eq!(both.user, "cli-user");

        let file_only = without_env(&cli(None, None), &from_file());
        assert_eq!(file_only.port, 2);
        assert_eq!(file_only.dbname, "file-db");
        assert_eq!(file_only.user, "file-user");

        let neither = without_env(&cli(None, None), &FileConfig::default());
        assert_eq!(neither.port, DEFAULT_PORT);
        assert_eq!(neither.dbname, DEFAULT_DBNAME);
        assert_eq!(neither.user, DEFAULT_USER);
    }

    #[test]
    fn config_path_lives_under_the_crt_query_directory() {
        let path = config_path_in(Path::new("/home/u/.config"));
        assert_eq!(path, PathBuf::from("/home/u/.config/crt-query/config.toml"));
    }

    #[test]
    fn a_valid_config_file_parses() {
        let cfg: FileConfig = toml::from_str(
            "host = \"db.internal\"\nport = 6432\ndbname = \"certwatch\"\nuser = \"guest\"\n",
        )
        .unwrap();
        assert_eq!(cfg.host.as_deref(), Some("db.internal"));
        assert_eq!(cfg.port, Some(6432));
    }

    #[test]
    fn an_unknown_key_is_rejected() {
        assert!(toml::from_str::<FileConfig>("host = \"x\"\nhosts = [\"y\"]\n").is_err());
    }

    #[test]
    fn a_wrong_type_is_rejected() {
        assert!(toml::from_str::<FileConfig>("port = \"5432\"\n").is_err());
    }
}
