//! Servers saved in Postico 2, read from its libraries. Passwords stay in the
//! Keychain; the target only says where to find one, and `saved_password`
//! asks for it when the user connects.

use std::path::{Path, PathBuf};

use serde_json::Value;
use tokio_postgres::Config;

use super::{Source, Target};

fn postico_dir() -> Option<PathBuf> {
    let dir =
        dirs::home_dir()?.join("Library/Containers/at.eggerapps.Postico/Data/Library/Application Support/Postico");
    dir.is_dir().then_some(dir)
}

/// Every saved server that Scarlet can reach directly (servers behind an SSH tunnel are left out).
pub fn targets() -> Vec<Target> {
    let Some(dir) = postico_dir() else { return Vec::new() };
    let mut targets = Vec::new();
    for library in libraries(&dir) {
        collect(&library, 0, &mut targets);
    }
    targets
}

/// The library folders Postico knows about, or its default one.
fn libraries(dir: &Path) -> Vec<PathBuf> {
    let listed: Vec<PathBuf> = std::fs::read_to_string(dir.join("LibraryLocations.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|json| json["libraries"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|library| library["url"].as_str()?.strip_prefix("file://").map(percent_decode))
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
        .collect();
    if listed.is_empty() { vec![dir.join("Local Library")] } else { listed }
}

/// Servers in `folder` and the folders inside it.
fn collect(folder: &Path, depth: usize, out: &mut Vec<Target>) {
    let Ok(entries) = std::fs::read_dir(folder) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if let Some(target) = std::fs::read_to_string(path.join("properties.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|json| server(&json))
        {
            out.push(target);
        } else if depth < 4 {
            collect(&path, depth + 1, out);
        }
    }
}

fn server(json: &Value) -> Option<Target> {
    let postgres = json["connection"]["postgres"].as_object()?;
    if json["connection"]["ssh"]["enabled"] == true {
        return None;
    }
    let text = |key: &str| postgres.get(key).and_then(Value::as_str).filter(|s| !s.is_empty());
    let mut config = Config::new();
    config.host(text("host")?);
    if let Some(port) = postgres.get("port").and_then(Value::as_u64).and_then(|p| u16::try_from(p).ok()) {
        config.port(port);
    }
    if let Some(database) = text("database") {
        config.dbname(database);
    }
    if let Some(user) = text("user") {
        config.user(user);
    }
    let password = postgres.get("password").unwrap_or(&Value::Null);
    let keychain = match (password["keychainItemService"].as_str(), password["keychainItemAccount"].as_str()) {
        (Some(service), Some(account)) => Some((service.to_owned(), account.to_owned())),
        _ => None,
    };
    let name = json["displayName"].as_str().unwrap_or("server").to_owned();
    Some(Target::new(config, Source::Postico { name, keychain }, false))
}

/// The saved server called `name`, with its saved password when the Keychain gives it.
pub fn named(name: &str) -> Option<Target> {
    let target = targets()
        .into_iter()
        .find(|t| matches!(&t.source, Source::Postico { name: n, .. } if n.eq_ignore_ascii_case(name)))?;
    if target.has_password() {
        return Some(target);
    }
    Some(saved_password(&target.source).map_or_else(|| target.clone(), |p| target.with_password(&p)))
}

/// The password Postico saved for `source`, asking the Keychain (which may
/// ask the user to allow it).
#[cfg(target_os = "macos")]
pub fn saved_password(source: &Source) -> Option<String> {
    let Source::Postico { keychain: Some((service, account)), .. } = source else { return None };
    let bytes = security_framework::passwords::get_generic_password(service, account).ok()?;
    String::from_utf8(bytes).ok()
}

#[cfg(not(target_os = "macos"))]
pub fn saved_password(_: &Source) -> Option<String> {
    None
}

/// `Local%20Library` -> `Local Library`.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = text.get(i + 1..i + 3).and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
