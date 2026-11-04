//! What the agent may see of a database's values, and the masking that hides
//! personal data and secrets before anything leaves this computer.

use std::{collections::HashMap, sync::LazyLock};

use regex::Regex;
use serde_json::Value;

use crate::connect::Target;

/// How much of the data Ask shares with the agent, chosen per database.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Visibility {
    /// Table and column names only.
    Schema,
    /// Values, with personal data and secrets replaced.
    Masked,
    /// Everything.
    Full,
}

impl Visibility {
    pub const ALL: [Visibility; 3] = [Visibility::Masked, Visibility::Schema, Visibility::Full];

    pub fn key(self) -> &'static str {
        match self {
            Visibility::Schema => "schema",
            Visibility::Masked => "masked",
            Visibility::Full => "full",
        }
    }

    pub fn parse(key: &str) -> Option<Visibility> {
        Visibility::ALL.into_iter().find(|v| v.key() == key)
    }

    pub fn label(self) -> &'static str {
        match self {
            Visibility::Masked => "Masked values",
            Visibility::Schema => "Table layout only",
            Visibility::Full => "All values",
        }
    }

    pub fn explanation(self) -> &'static str {
        match self {
            Visibility::Masked => {
                "Claude can look at values, but emails, names, phone numbers, addresses, ID and card numbers, \
                 IP addresses and secrets are replaced before they leave this computer."
            }
            Visibility::Schema => "Claude sees tables and columns but no values. Answers are a little less accurate.",
            Visibility::Full => "Claude sees values as they are. For your own test data.",
        }
    }

    /// For the question box: what Claude sees of `database`.
    pub fn summary(self, database: &str) -> String {
        match self {
            Visibility::Masked => format!("Claude sees {database}'s tables and masked values."),
            Visibility::Schema => format!("Claude sees {database}'s tables, but no values."),
            Visibility::Full => format!("Claude sees {database}'s tables and all values."),
        }
    }
}

/// The choice for each database, kept in Scarlet's config folder.
pub fn saved(target: &Target) -> Option<Visibility> {
    let choices = read_choices();
    Visibility::parse(choices.get(&choice_key(target))?.as_str()?)
}

pub fn save(target: &Target, visibility: Visibility) {
    let Some(path) = choices_path() else { return };
    let mut choices = read_choices();
    choices.insert(choice_key(target), Value::String(visibility.key().into()));
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string_pretty(&Value::Object(choices)) {
        let _ = std::fs::write(path, text);
    }
}

fn choices_path() -> Option<std::path::PathBuf> {
    Some(dirs::config_dir()?.join("scarlet").join("ask.json"))
}

fn read_choices() -> serde_json::Map<String, Value> {
    choices_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

fn choice_key(target: &Target) -> String {
    let (host, port, database) = target.key();
    format!("{host}:{port}/{database}")
}

/// Replaces sensitive values with stand-ins such as `‹email 3›`. The same value
/// always gets the same stand-in, so the agent can still count and group.
#[derive(Default)]
pub struct Masker {
    stand_ins: HashMap<(&'static str, String), usize>,
    counts: HashMap<&'static str, usize>,
}

impl Masker {
    /// A value from a column: hidden whole when the column is sensitive,
    /// otherwise searched for personal data inside it.
    pub fn cell(&mut self, kind: Option<&'static str>, value: &str) -> String {
        match kind {
            // A yes/no flag such as `email_verified` gives nothing away.
            Some(_) if matches!(value, "t" | "f" | "true" | "false") => value.to_owned(),
            Some(kind) => self.stand_in(kind, value),
            None => self.text(value),
        }
    }

    /// Free text with emails, personal identity numbers, international phone
    /// numbers and card numbers replaced.
    pub fn text(&mut self, text: &str) -> String {
        if let Ok(json) = serde_json::from_str::<Value>(text)
            && (json.is_object() || json.is_array())
        {
            return self.json(json).to_string();
        }
        let mut out = text.to_owned();
        for (kind, pattern) in PATTERNS.iter() {
            if !pattern.is_match(&out) {
                continue;
            }
            let mut replaced = String::new();
            let mut last = 0;
            for found in pattern.find_iter(&out) {
                if *kind == "card number" && !luhn(found.as_str()) {
                    continue;
                }
                replaced.push_str(&out[last..found.start()]);
                replaced.push_str(&self.stand_in(kind, found.as_str()));
                last = found.end();
            }
            replaced.push_str(&out[last..]);
            out = replaced;
        }
        out
    }

    /// JSON with sensitive keys hidden and every string searched.
    pub fn json(&mut self, value: Value) -> Value {
        match value {
            Value::Object(map) => Value::Object(
                map.into_iter()
                    .map(|(key, value)| {
                        let value = match (sensitive_column(&key, ""), &value) {
                            (Some(kind), Value::String(s)) => Value::String(self.stand_in(kind, s)),
                            (Some(kind), Value::Number(n)) => Value::String(self.stand_in(kind, &n.to_string())),
                            (Some(_), Value::Null) => Value::Null,
                            (Some(kind), other) => Value::String(self.stand_in(kind, &other.to_string())),
                            (None, _) => self.json(value),
                        };
                        (key, value)
                    })
                    .collect(),
            ),
            Value::Array(items) => Value::Array(items.into_iter().map(|item| self.json(item)).collect()),
            Value::String(s) => Value::String(self.text(&s)),
            other => other,
        }
    }

    fn stand_in(&mut self, kind: &'static str, value: &str) -> String {
        let next = self.counts.get(kind).copied().unwrap_or(0) + 1;
        let number = *self.stand_ins.entry((kind, value.to_owned())).or_insert_with(|| {
            self.counts.insert(kind, next);
            next
        });
        format!("‹{kind} {number}›")
    }
}

static PATTERNS: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    [
        ("email", r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}"),
        // Swedish personnummer and similar: date digits, a separator, four digits.
        ("id number", r"\b(?:19|20)?\d{6}[-+]\d{4}\b"),
        ("phone", r"\+\d[\d \-]{6,15}\d"),
        ("card number", r"\b\d(?:[ -]?\d){12,18}\b"),
    ]
    .into_iter()
    .map(|(kind, pattern)| (kind, Regex::new(pattern).expect("valid pattern")))
    .collect()
});

fn luhn(number: &str) -> bool {
    let digits: Vec<u32> = number.chars().filter_map(|c| c.to_digit(10)).collect();
    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &d)| if i % 2 == 1 { if d * 2 > 9 { d * 2 - 9 } else { d * 2 } } else { d })
        .sum();
    digits.len() >= 13 && sum.is_multiple_of(10)
}

/// What a column holds when its name or type says it is personal or secret.
pub fn sensitive_column(name: &str, type_name: &str) -> Option<&'static str> {
    if matches!(type_name, "inet" | "cidr" | "macaddr" | "macaddr8") {
        return Some("ip address");
    }
    let words = words(name);
    let has = |w: &str| words.iter().any(|word| word == w);
    let joined = words.concat();
    let rules: [(&str, bool); 10] = [
        (
            "secret",
            ["password", "passwd", "secret", "apikey", "credential"].iter().any(|w| joined.contains(w))
                || ["pwd", "token", "salt", "hash", "otp"].iter().any(|w| has(w)),
        ),
        ("email", has("email") || has("mail") || joined.contains("email")),
        ("phone", ["phone", "mobile", "tel", "telephone", "fax", "msisdn", "cellphone"].iter().any(|w| has(w))),
        (
            "id number",
            ["ssn", "personnummer", "pnr", "passport", "nationalid", "taxid", "socialsecurity"]
                .iter()
                .any(|w| has(w) || joined.contains(w)),
        ),
        (
            "card number",
            ["iban", "bic", "card", "cc", "cvv", "cvc", "pan", "bankaccount", "accountnumber"]
                .iter()
                .any(|w| has(w) || joined == *w),
        ),
        ("birth date", ["dob", "birth", "birthday", "birthdate", "born"].iter().any(|w| has(w))),
        ("address", ["address", "addr", "street", "zip", "zipcode", "postal", "postcode"].iter().any(|w| has(w))),
        ("location", ["lat", "lng", "lon", "latitude", "longitude", "coordinates", "geo"].iter().any(|w| has(w))),
        ("ip address", has("ip") || joined == "ipaddress"),
        ("name", is_person_name(&words)),
    ];
    rules.into_iter().find(|(_, hit)| *hit).map(|(kind, _)| kind)
}

/// `name`, `first_name`, `username`… but not `table_name` or `product_name`.
fn is_person_name(words: &[String]) -> bool {
    const NOT_PEOPLE: &[&str] = &[
        "file",
        "table",
        "schema",
        "column",
        "host",
        "domain",
        "product",
        "item",
        "company",
        "org",
        "organization",
        "brand",
        "category",
        "event",
        "type",
        "kind",
        "tag",
        "app",
        "service",
        "model",
        "plan",
        "feature",
        "role",
        "group",
        "team",
        "project",
        "field",
        "key",
        "class",
        "package",
        "country",
        "city",
        "street",
        "store",
        "shop",
        "site",
        "bucket",
        "queue",
        "job",
        "task",
        "metric",
        "setting",
        "status",
        "state",
        "region",
        "currency",
    ];
    let named = words.iter().any(|w| matches!(w.as_str(), "name" | "surname" | "firstname" | "lastname" | "username"));
    named && !words.iter().any(|w| NOT_PEOPLE.contains(&w.as_str()))
}

/// `customerEmail2` -> ["customer", "email"]; `first_name` -> ["first", "name"].
fn words(name: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut previous_lower = false;
    for c in name.chars() {
        if !c.is_alphabetic() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            previous_lower = false;
            continue;
        }
        if c.is_uppercase() && previous_lower && !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
        previous_lower = c.is_lowercase();
        current.extend(c.to_lowercase());
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spots_personal_columns_but_not_ordinary_ones() {
        for personal in
            ["email", "customerEmail", "first_name", "name", "phone_number", "password_hash", "personnummer"]
        {
            assert!(sensitive_column(personal, "text").is_some(), "{personal} should be masked");
        }
        for ordinary in ["status", "product_name", "table_name", "total", "created_at", "country", "kind"] {
            assert!(sensitive_column(ordinary, "text").is_none(), "{ordinary} should be shown");
        }
        assert_eq!(sensitive_column("client", "inet"), Some("ip address"));
    }

    #[test]
    fn hides_personal_data_inside_free_text_and_json() {
        let mut masker = Masker::default();
        let note = masker.text("Call +46 70 123 45 67 or mail anna@example.com about 19850612-1234");
        assert!(!note.contains("anna@") && !note.contains("123 45") && !note.contains("1234"), "{note}");
        assert!(note.contains("‹email 1›"));
        // Ordinary numbers, such as timestamps and amounts, are left alone.
        assert_eq!(masker.text("1730716800 and 4299.50"), "1730716800 and 4299.50");

        let json = masker.text(r#"{"plan": "pro", "contact": {"email": "anna@example.com"}}"#);
        assert_eq!(json, r#"{"plan":"pro","contact":{"email":"‹email 1›"}}"#);
    }
}
