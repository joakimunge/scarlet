//! The SQL editor's text, kept per database between sessions so nothing typed is lost.

use std::path::PathBuf;

use crate::connect::Target;

fn path(target: &Target) -> Option<PathBuf> {
    let (host, port, database) = target.key();
    let name: String = format!("{host}-{port}-{database}")
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    Some(dirs::data_dir()?.join("scarlet").join("scratch").join(format!("{name}.sql")))
}

pub fn load(target: &Target) -> String {
    path(target).and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default()
}

/// Best effort: an unwritable data directory just means the text isn't kept.
pub fn save(target: &Target, text: &str) {
    let Some(path) = path(target) else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, text);
}
