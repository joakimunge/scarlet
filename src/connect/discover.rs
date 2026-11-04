//! Finding databases without setup: .env files, the PG* environment, running
//! Docker containers and servers listening on this machine.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use serde_json::Value;
use tokio::{net::TcpStream, process::Command, sync::mpsc::UnboundedSender, task::JoinSet, time::timeout};
use tokio_postgres::Config;

use super::{SOCKET_DIRS, Source, Target, is_url, os_user, ssl_mode};
use crate::{app::Msg, db::postgres};

const ENV_FILES: &[&str] = &[".env", ".env.local", ".env.development", ".env.development.local"];
const TCP_PORTS: &[u16] = &[5432, 5433, 5434, 5435, 5436, 5437, 5438, 5439, 15432, 54320, 54321, 54322];

/// Postgres URLs in .env files from the working directory up to the project root.
pub fn dotenv_targets(start: &Path) -> Vec<Target> {
    let mut targets = Vec::new();
    for dir in project_dirs(start) {
        for name in ENV_FILES {
            let path = dir.join(name);
            let Ok(entries) = dotenvy::from_path_iter(&path) else { continue };
            let file = if dir == start {
                (*name).to_owned()
            } else {
                let parent = dir.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
                format!("{parent}/{name}")
            };
            for (key, value) in entries.flatten() {
                if !is_url(&value) {
                    continue;
                }
                if let Ok(target) = Target::parse(&value, Source::DotEnv { file: file.clone(), key }) {
                    targets.push(target);
                }
            }
        }
    }
    targets
}

fn project_dirs(start: &Path) -> Vec<PathBuf> {
    let home = dirs::home_dir();
    let mut dirs = Vec::new();
    for dir in start.ancestors().take(6) {
        if Some(dir) == home.as_deref() || dir.parent().is_none() {
            break;
        }
        dirs.push(dir.to_path_buf());
        if dir.join(".git").exists() {
            break;
        }
    }
    dirs
}

/// DATABASE_URL, or a target built from PGHOST / PGPORT / PGDATABASE / PGUSER.
pub fn env_target() -> Option<Target> {
    let var = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());
    if let Some(url) = var("DATABASE_URL").filter(|u| is_url(u)) {
        return Target::parse(&url, Source::Environment).ok();
    }
    if ["PGHOST", "PGPORT", "PGDATABASE", "PGUSER"].iter().all(|k| var(k).is_none()) {
        return None;
    }
    let mut config = Config::new();
    if let Some(hosts) = var("PGHOST") {
        for host in hosts.split(',') {
            if host.starts_with('/') {
                config.host_path(host);
            } else {
                config.host(host);
            }
        }
    }
    if let Some(port) = var("PGPORT").and_then(|p| p.parse().ok()) {
        config.port(port);
    }
    if let Some(database) = var("PGDATABASE") {
        config.dbname(database);
    }
    if let Some(user) = var("PGUSER") {
        config.user(user);
    }
    let mode = var("PGSSLMODE");
    if let Some(mode) = &mode {
        config.ssl_mode(ssl_mode(mode));
    }
    let verify = matches!(mode.as_deref(), Some("verify-ca" | "verify-full"));
    Some(Target::new(config, Source::Environment, verify))
}

struct Server {
    config: Config,
    /// Roles to try, in order, when listing databases.
    users: Vec<String>,
    source: Source,
}

/// Looks for servers in the background, reporting each server's databases as they come in.
pub fn spawn(tx: UnboundedSender<Msg>) {
    tokio::spawn(async move {
        let (docker, sockets, open_ports) = tokio::join!(docker_servers(), socket_ports(), open_tcp_ports());
        let mut servers = docker;
        let local_users = vec![os_user(), "postgres".to_owned()];
        let mut seen: HashSet<u16> = HashSet::new();
        for (dir, port) in sockets {
            if seen.insert(port) {
                let mut config = Config::new();
                config.host_path(dir).port(port);
                servers.push(Server { config, users: local_users.clone(), source: Source::Local });
            }
        }
        let docker_ports: HashSet<u16> = servers
            .iter()
            .filter(|s| matches!(s.source, Source::Docker { .. }))
            .filter_map(|s| s.config.get_ports().first().copied())
            .collect();
        for port in open_ports {
            if !seen.contains(&port) && !docker_ports.contains(&port) {
                let mut config = Config::new();
                config.host("localhost").port(port);
                servers.push(Server { config, users: local_users.clone(), source: Source::Local });
            }
        }

        let mut probes = JoinSet::new();
        for server in servers {
            let tx = tx.clone();
            probes.spawn(async move {
                let _ = tx.send(Msg::Discovered(probe(server).await));
            });
        }
        while probes.join_next().await.is_some() {}
        let _ = tx.send(Msg::DiscoveryDone);
    });
}

/// Every database on the server for the first role that can log in. If none can,
/// the server is still offered so that connecting can ask for a password.
async fn probe(server: Server) -> Vec<Target> {
    let mut fallback = None;
    for user in &server.users {
        let mut config = server.config.clone();
        config.user(user);
        let target = Target::new(config, server.source.clone(), false);
        let listing = target.with_database("postgres");
        match timeout(Duration::from_secs(3), postgres::list_databases(&listing)).await {
            Ok(Ok(databases)) if !databases.is_empty() => {
                return databases.iter().map(|db| target.with_database(db)).collect();
            }
            _ => {
                fallback.get_or_insert(target);
            }
        }
    }
    fallback.into_iter().collect()
}

async fn socket_ports() -> Vec<(&'static str, u16)> {
    let mut found = Vec::new();
    for dir in SOCKET_DIRS {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(port) = name.to_str().and_then(|n| n.strip_prefix(".s.PGSQL.")) else { continue };
            if let Ok(port) = port.parse() {
                found.push((*dir, port));
            }
        }
    }
    found.sort_by_key(|(_, port)| *port);
    found
}

async fn open_tcp_ports() -> Vec<u16> {
    let mut checks = JoinSet::new();
    for &port in TCP_PORTS {
        checks.spawn(async move {
            let open = timeout(Duration::from_millis(250), TcpStream::connect(("127.0.0.1", port))).await;
            matches!(open, Ok(Ok(_))).then_some(port)
        });
    }
    let mut ports: Vec<u16> = checks.join_all().await.into_iter().flatten().collect();
    ports.sort_unstable();
    ports
}

async fn docker_servers() -> Vec<Server> {
    let Some(ids) = docker(&["ps", "-q"]).await else { return Vec::new() };
    let ids: Vec<&str> = ids.split_whitespace().collect();
    if ids.is_empty() {
        return Vec::new();
    }
    let mut args = vec!["inspect"];
    args.extend(ids);
    let Some(json) = docker(&args).await else { return Vec::new() };
    let Ok(Value::Array(containers)) = serde_json::from_str(&json) else { return Vec::new() };
    containers.iter().filter_map(docker_server).collect()
}

/// A container that publishes Postgres' port, with credentials from its environment.
fn docker_server(container: &Value) -> Option<Server> {
    let name = container["Name"].as_str()?.trim_start_matches('/').to_owned();
    let binding = container["NetworkSettings"]["Ports"]["5432/tcp"].as_array()?.first()?;
    let port: u16 = binding["HostPort"].as_str()?.parse().ok()?;
    let env: HashMap<&str, &str> = container["Config"]["Env"]
        .as_array()
        .map(|vars| vars.iter().filter_map(|v| v.as_str()?.split_once('=')).collect())
        .unwrap_or_default();
    let user = env.get("POSTGRES_USER").copied().unwrap_or("postgres");
    let mut config = Config::new();
    config.host("localhost").port(port).user(user).dbname(env.get("POSTGRES_DB").copied().unwrap_or(user));
    if let Some(password) = env.get("POSTGRES_PASSWORD") {
        config.password(*password);
    }
    Some(Server { config, users: vec![user.to_owned()], source: Source::Docker { container: name } })
}

async fn docker(args: &[&str]) -> Option<String> {
    let output =
        Command::new("docker").args(args).stdin(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true).output();
    let output = timeout(Duration::from_secs(2), output).await.ok()?.ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}
