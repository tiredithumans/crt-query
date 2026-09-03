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
/// absent fields fall back to CLI flags, then to the built-in defaults.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub dbname: Option<String>,
    pub user: Option<String>,
    pub db_url: Option<String>,
}

/// Fully resolved connection settings, after CLI flags and the config file
/// have been folded into the built-in defaults.
#[derive(Debug, Clone)]
pub struct Conn {
    pub host: String,
    pub port: u16,
    pub dbname: String,
    pub user: String,
    pub db_url: Option<String>,
}

/// Where the config file is read from, if a location can be determined at all.
pub fn config_path() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let appdata = std::env::var_os("APPDATA")?;
        Some(config_path_in(&PathBuf::from(appdata)))
    }
    #[cfg(not(windows))]
    {
        let root = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(config_path_in(&root))
    }
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

/// Fold CLI flags, the config file and built-in defaults into one connection.
///
/// Precedence, highest first: CLI flag, config file, built-in default. A
/// `db_url` from either source overrides the individual host/port/dbname/user
/// settings entirely, mirroring what `--db-url` does on the command line.
pub fn resolve(cli: &ConnOpts, file: &FileConfig) -> Conn {
    let db_url = cli.db_url.clone().or_else(|| file.db_url.clone());
    Conn {
        host: cli
            .host
            .clone()
            .or_else(|| file.host.clone())
            .unwrap_or_else(|| DEFAULT_HOST.into()),
        port: cli.port.or(file.port).unwrap_or(DEFAULT_PORT),
        dbname: cli
            .dbname
            .clone()
            .or_else(|| file.dbname.clone())
            .unwrap_or_else(|| DEFAULT_DBNAME.into()),
        user: cli
            .user
            .clone()
            .or_else(|| file.user.clone())
            .unwrap_or_else(|| DEFAULT_USER.into()),
        db_url,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            port: None,
            dbname: None,
            user: None,
            db_url: db_url.map(str::to_string),
        }
    }

    #[test]
    fn defaults_apply_when_nothing_is_set() {
        let conn = resolve(&cli(None, None), &FileConfig::default());
        assert_eq!(conn.host, DEFAULT_HOST);
        assert_eq!(conn.port, DEFAULT_PORT);
        assert_eq!(conn.dbname, DEFAULT_DBNAME);
        assert_eq!(conn.user, DEFAULT_USER);
        assert!(conn.db_url.is_none());
    }

    #[test]
    fn cli_beats_the_config_file() {
        let conn = resolve(
            &cli(Some("cli.example"), None),
            &file(Some("file.example"), None),
        );
        assert_eq!(conn.host, "cli.example");
    }

    #[test]
    fn config_file_beats_the_defaults() {
        let conn = resolve(&cli(None, None), &file(Some("file.example"), None));
        assert_eq!(conn.host, "file.example");
    }

    #[test]
    fn a_cli_db_url_overrides_everything_else() {
        let conn = resolve(
            &cli(
                Some("ignored.example"),
                Some("postgresql://u@cli.example/db"),
            ),
            &file(Some("also-ignored.example"), None),
        );
        assert_eq!(
            conn.db_url.as_deref(),
            Some("postgresql://u@cli.example/db")
        );
    }

    #[test]
    fn a_config_file_db_url_overrides_the_individual_fields() {
        let conn = resolve(
            &cli(Some("ignored.example"), None),
            &file(None, Some("postgresql://u@f.example/db")),
        );
        assert_eq!(conn.db_url.as_deref(), Some("postgresql://u@f.example/db"));
    }

    #[test]
    fn a_cli_db_url_beats_a_config_file_db_url() {
        let conn = resolve(
            &cli(None, Some("postgresql://u@cli.example/db")),
            &file(None, Some("postgresql://u@f.example/db")),
        );
        assert_eq!(
            conn.db_url.as_deref(),
            Some("postgresql://u@cli.example/db")
        );
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
