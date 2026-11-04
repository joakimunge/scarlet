//! Password lookup in ~/.pgpass (or $PGPASSFILE), following libpq's format.

use std::path::PathBuf;

pub fn lookup(host: &str, port: u16, database: &str, user: &str) -> Option<String> {
    let path = std::env::var_os("PGPASSFILE")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".pgpass")))?;
    let text = std::fs::read_to_string(path).ok()?;
    find(&text, host, &port.to_string(), database, user)
}

fn find(text: &str, host: &str, port: &str, database: &str, user: &str) -> Option<String> {
    let matches = |pattern: &str, value: &str| pattern == "*" || pattern == value;
    text.lines().filter(|line| !line.trim_start().starts_with('#')).find_map(|line| {
        let fields = split(line);
        let [h, p, d, u, password] = fields.as_slice() else { return None };
        (matches(h, host) && matches(p, port) && matches(d, database) && matches(u, user)).then(|| password.clone())
    })
}

/// Splits on unescaped colons; the fifth field takes the rest of the line.
fn split(line: &str) -> Vec<String> {
    let mut fields = vec![String::new()];
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(next) = chars.next() {
                    fields.last_mut().unwrap().push(next);
                }
            }
            ':' if fields.len() < 5 => fields.push(String::new()),
            _ => fields.last_mut().unwrap().push(c),
        }
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_wildcards_and_escaped_colons() {
        let file = "# comment\nother:5432:*:*:nope\nlocalhost:*:shop:app:pa\\:ss:word\n";
        assert_eq!(find(file, "localhost", "5432", "shop", "app").as_deref(), Some("pa:ss:word"));
        assert_eq!(find(file, "localhost", "5432", "blog", "app"), None);
    }
}
