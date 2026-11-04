//! Connection targets: where they come from and how they are completed with
//! the same defaults libpq would apply.

pub mod discover;
mod pgpass;

use std::{path::Path, time::Duration};

use tokio_postgres::config::{Config, Host, SslMode};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Argument,
    Pasted,
    DotEnv { file: String, key: String },
    Environment,
    Docker { container: String },
    Local,
}

impl Source {
    pub fn label(&self) -> String {
        match self {
            Source::Argument => "argument".into(),
            Source::Pasted => "pasted".into(),
            Source::DotEnv { file, key } => format!("{file} {key}"),
            Source::Environment => "environment".into(),
            Source::Docker { container } => format!("docker {container}"),
            Source::Local => "local".into(),
        }
    }

    /// Lower ranks are listed first.
    fn rank(&self) -> u8 {
        match self {
            Source::Argument | Source::Pasted => 0,
            Source::DotEnv { .. } => 1,
            Source::Environment => 2,
            Source::Docker { .. } => 3,
            Source::Local => 4,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Target {
    pub config: Config,
    pub source: Source,
    /// Verify server certificates (sslmode verify-ca / verify-full).
    pub verify_tls: bool,
}

impl Target {
    /// Parses a postgres:// URL, a libpq key=value string, or a bare database name.
    pub fn parse(input: &str, source: Source) -> Result<Target, String> {
        let input = input.trim();
        if !is_url(input) && !input.contains('=') && !input.contains(char::is_whitespace) {
            let mut config = Config::new();
            config.dbname(input);
            return Ok(Target::new(config, source, false));
        }
        let (cleaned, verify_tls) = sanitize_url(input);
        let config: Config = cleaned.parse().map_err(|e: tokio_postgres::Error| e.to_string())?;
        Ok(Target::new(config, source, verify_tls))
    }

    pub fn new(mut config: Config, source: Source, verify_tls: bool) -> Target {
        complete(&mut config);
        Target { config, source, verify_tls }
    }

    pub fn database(&self) -> &str {
        self.config.get_dbname().unwrap_or_default()
    }

    pub fn user(&self) -> &str {
        self.config.get_user().unwrap_or_default()
    }

    pub fn host(&self) -> String {
        match self.config.get_hosts().first() {
            Some(Host::Tcp(host)) => host.clone(),
            _ => "localhost".into(),
        }
    }

    pub fn port(&self) -> u16 {
        self.config.get_ports().first().copied().unwrap_or(5432)
    }

    /// `user@host:port`, the way people say where a database lives.
    pub fn address(&self) -> String {
        format!("{}@{}:{}", self.user(), self.host(), self.port())
    }

    pub fn has_password(&self) -> bool {
        self.config.get_password().is_some()
    }

    /// Identity used to drop duplicates found by several sources.
    pub fn key(&self) -> (String, u16, String) {
        let host = match self.host().as_str() {
            "127.0.0.1" | "::1" => "localhost".to_owned(),
            other => other.to_owned(),
        };
        (host, self.port(), self.database().to_owned())
    }

    pub fn sort_key(&self) -> (u8, String, u16, bool, String) {
        let (host, port, database) = self.key();
        (self.source.rank(), host, port, database == "postgres", database)
    }

    pub fn with_database(&self, database: &str) -> Target {
        let mut config = self.config.clone();
        config.dbname(database);
        Target { config, source: self.source.clone(), verify_tls: self.verify_tls }
    }

    pub fn with_password(&self, password: &str) -> Target {
        let mut config = self.config.clone();
        config.password(password);
        Target { config, source: self.source.clone(), verify_tls: self.verify_tls }
    }
}

pub fn is_url(value: &str) -> bool {
    let value = value.trim();
    value.starts_with("postgres://") || value.starts_with("postgresql://")
}

pub fn os_user() -> String {
    std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_else(|_| "postgres".into())
}

pub fn ssl_mode(mode: &str) -> SslMode {
    match mode {
        "disable" | "allow" => SslMode::Disable,
        "require" | "verify-ca" | "verify-full" => SslMode::Require,
        _ => SslMode::Prefer,
    }
}

/// Parameters tokio-postgres understands; anything else (Prisma's `schema`,
/// `pgbouncer`, ...) would make parsing fail, so it is dropped.
const KNOWN_PARAMS: &[&str] = &[
    "user",
    "password",
    "dbname",
    "options",
    "application_name",
    "host",
    "hostaddr",
    "port",
    "connect_timeout",
    "tcp_user_timeout",
    "keepalives",
    "keepalives_idle",
    "keepalives_interval",
    "keepalives_retries",
    "target_session_attrs",
    "channel_binding",
    "load_balance_hosts",
    "sslnegotiation",
];

/// Normalises a URL's query string. Returns the cleaned input and whether
/// certificate verification was requested.
fn sanitize_url(input: &str) -> (String, bool) {
    if !is_url(input) {
        return (input.to_owned(), false);
    }
    let Some((base, query)) = input.split_once('?') else {
        return (input.to_owned(), false);
    };
    let mut verify = false;
    let mut kept = Vec::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key == "sslmode" {
            verify = matches!(value, "verify-ca" | "verify-full");
            let mode = match ssl_mode(value) {
                SslMode::Disable => "disable",
                SslMode::Require => "require",
                _ => "prefer",
            };
            kept.push(format!("sslmode={mode}"));
        } else if KNOWN_PARAMS.contains(&key) {
            kept.push(pair.to_owned());
        }
    }
    if kept.is_empty() { (base.to_owned(), verify) } else { (format!("{base}?{}", kept.join("&")), verify) }
}

/// Fills in what libpq would assume: OS user, database named after the user,
/// the local socket, a password from PGPASSWORD or ~/.pgpass.
fn complete(config: &mut Config) {
    if config.get_user().is_none() {
        config.user(os_user());
    }
    if config.get_dbname().is_none() {
        let user = config.get_user().unwrap_or_default().to_owned();
        config.dbname(user);
    }
    let port = config.get_ports().first().copied().unwrap_or(5432);
    if config.get_hosts().is_empty() {
        match default_socket_dir(port) {
            Some(dir) => config.host_path(dir),
            None => config.host("localhost"),
        };
    }
    if config.get_connect_timeout().is_none() {
        config.connect_timeout(Duration::from_secs(5));
    }
    if config.get_application_name().is_none() {
        config.application_name("scarlet");
    }
    if config.get_password().is_none() {
        let host = match config.get_hosts().first() {
            Some(Host::Tcp(host)) => host.clone(),
            _ => "localhost".into(),
        };
        let user = config.get_user().unwrap_or_default().to_owned();
        let database = config.get_dbname().unwrap_or_default().to_owned();
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|p| !p.is_empty())
            .or_else(|| pgpass::lookup(&host, port, &database, &user));
        if let Some(password) = password {
            config.password(password);
        }
    }
}

pub const SOCKET_DIRS: &[&str] = &["/tmp", "/var/run/postgresql"];

fn default_socket_dir(port: u16) -> Option<&'static str> {
    SOCKET_DIRS.iter().copied().find(|dir| Path::new(dir).join(format!(".s.PGSQL.{port}")).exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_parameters_the_driver_rejects() {
        let target = Target::parse(
            "postgres://app:secret@db.example.com:6543/shop?schema=public&pgbouncer=true&sslmode=verify-full",
            Source::Argument,
        )
        .unwrap();
        assert_eq!(target.address(), "app@db.example.com:6543");
        assert_eq!(target.database(), "shop");
        assert!(target.verify_tls);
        assert_eq!(target.config.get_ssl_mode(), SslMode::Require);
    }
}
