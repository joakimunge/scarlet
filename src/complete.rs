//! Turning a completion context into ranked suggestions from the schema:
//! tables where a table goes, the statement's columns where a column goes,
//! keywords in the case the user is typing.

use std::collections::{HashMap, HashSet};

use crate::{
    db::{Schema, ident},
    sql::{self, Expect, TableRef},
    ui::editor::{Popup, Suggestion},
};

/// Columns by (schema, table), each as (name, type).
pub type Columns = HashMap<(String, String), Vec<(String, String)>>;

const LIMIT: usize = 50;

struct Candidate {
    /// What the user types to match, e.g. a column's plain name.
    name: String,
    /// What gets inserted, quoted when needed.
    insert: String,
    detail: String,
    /// Lower comes first among equally good matches.
    rank: u8,
}

/// Suggestions at `cursor`, or `None` when there is nothing worth showing.
/// Without `force` nothing is offered for an empty word (except after a dot).
pub fn suggest(text: &str, cursor: usize, force: bool, schemas: &[Schema], columns: &Columns) -> Option<Popup> {
    let context = sql::completion_context(text, cursor)?;
    if context.prefix.is_empty() && context.qualifier.is_none() && !force {
        return None;
    }
    let mut candidates = Vec::new();
    match &context.qualifier {
        Some(qualifier) => {
            if let Some(table) = context
                .tables
                .iter()
                .find(|t| t.alias.as_deref() == Some(qualifier.as_str()))
                .or_else(|| context.tables.iter().find(|t| t.name == *qualifier))
            {
                add_columns(&mut candidates, table, schemas, columns, false);
            } else if let Some(schema) = schemas.iter().find(|s| s.name == *qualifier) {
                for relation in &schema.relations {
                    candidates.push(Candidate {
                        name: relation.name.clone(),
                        insert: ident(&relation.name),
                        detail: relation.kind.label().into(),
                        rank: 0,
                    });
                }
            } else {
                // A table named directly, not yet in a FROM clause.
                let table = TableRef { schema: None, name: qualifier.clone(), alias: None };
                add_columns(&mut candidates, &table, schemas, columns, false);
            }
        }
        None => match context.expect {
            Expect::Table => add_tables(&mut candidates, schemas, 0),
            Expect::Column => {
                let several = context.tables.len() > 1;
                for table in &context.tables {
                    add_columns(&mut candidates, table, schemas, columns, several);
                }
                add_keywords(&mut candidates, &context.prefix, 1);
            }
            Expect::Any => {
                add_keywords(&mut candidates, &context.prefix, 0);
                add_tables(&mut candidates, schemas, 1);
            }
        },
    }

    let items = rank(candidates, &context.prefix);
    // Nothing to offer when the word is already complete.
    if items.is_empty() || (items.len() == 1 && items[0].text.eq_ignore_ascii_case(&context.prefix)) {
        return None;
    }
    Some(Popup { items, selected: 0, start: context.start })
}

/// Candidates containing what was typed, those starting with it first. SQL
/// names are typed from the start, so loose letter-by-letter matching would
/// only add noise.
fn rank(candidates: Vec<Candidate>, prefix: &str) -> Vec<Suggestion> {
    let needle: Vec<char> = prefix.chars().flat_map(char::to_lowercase).collect();
    let mut found: Vec<(usize, Candidate)> = candidates
        .into_iter()
        .filter_map(|candidate| {
            let name: Vec<char> = candidate.name.chars().flat_map(char::to_lowercase).collect();
            let at = (0..=name.len().checked_sub(needle.len())?).find(|&i| name[i..i + needle.len()] == needle[..])?;
            Some((at, candidate))
        })
        .collect();
    // Nothing typed yet: keep the natural order (a table's columns as defined).
    if !needle.is_empty() {
        found.sort_by_key(|(at, candidate)| (*at > 0, candidate.rank, *at, candidate.name.len()));
    }
    let mut seen = HashSet::new();
    found
        .into_iter()
        .filter(|(_, candidate)| seen.insert(candidate.insert.clone()))
        .take(LIMIT)
        .map(|(at, candidate)| {
            // A quoted insert shifts the highlight by its opening quote.
            let at = at as u32 + u32::from(candidate.insert.starts_with('"'));
            Suggestion {
                text: candidate.insert,
                detail: candidate.detail,
                highlights: (at..at + needle.len() as u32).collect(),
            }
        })
        .collect()
}

fn add_tables(out: &mut Vec<Candidate>, schemas: &[Schema], rank: u8) {
    for schema in schemas {
        for relation in &schema.relations {
            let qualified = schema.name != "public";
            let insert = if qualified {
                format!("{}.{}", ident(&schema.name), ident(&relation.name))
            } else {
                ident(&relation.name)
            };
            let name = if qualified { format!("{}.{}", schema.name, relation.name) } else { relation.name.clone() };
            out.push(Candidate { name, insert, detail: relation.kind.label().into(), rank });
        }
        if schema.name != "public" {
            out.push(Candidate {
                name: schema.name.clone(),
                insert: ident(&schema.name),
                detail: "schema".into(),
                rank: rank + 1,
            });
        }
    }
}

fn add_columns(out: &mut Vec<Candidate>, table: &TableRef, schemas: &[Schema], columns: &Columns, label_table: bool) {
    let found = match &table.schema {
        Some(schema) => columns.get(&(schema.clone(), table.name.clone())),
        // Schemas are listed public first, like a default search path.
        None => schemas.iter().find_map(|s| columns.get(&(s.name.clone(), table.name.clone()))),
    };
    for (name, type_name) in found.into_iter().flatten() {
        let detail = if label_table {
            format!("{type_name} · {}", table.alias.as_ref().unwrap_or(&table.name))
        } else {
            type_name.clone()
        };
        out.push(Candidate { name: name.clone(), insert: ident(name), detail, rank: 0 });
    }
}

fn add_keywords(out: &mut Vec<Candidate>, prefix: &str, rank: u8) {
    // Match the case being typed: "sel" gets "select", "SEL" or nothing gets "SELECT".
    let lowercase = prefix.chars().any(char::is_lowercase) && !prefix.chars().any(char::is_uppercase);
    for keyword in sql::KEYWORDS {
        let insert = if lowercase { keyword.to_lowercase() } else { (*keyword).to_owned() };
        out.push(Candidate { name: keyword.to_lowercase(), insert, detail: "keyword".into(), rank });
    }
}
