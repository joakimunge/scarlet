//! The tools an agent gets for looking at a database: the schema, one table in
//! detail, and small read-only queries. Shared by the `scarlet mcp` server and
//! the bridge behind Scarlet's own Ask.

use std::fmt::Write as _;

use serde_json::{Value, json};

use super::mask::{Masker, Visibility, sensitive_column};
use crate::db::{
    Relation, RelationKind,
    postgres::{Outcome, Session},
};

/// Rows an exploratory query returns to the agent.
const QUERY_ROWS: usize = 50;
/// Longest value shown to the agent; longer ones are cut.
const VALUE_CHARS: usize = 200;
/// Beyond this the schema overview lists tables only.
const OVERVIEW_CHARS: usize = 60_000;

pub const SHOW_RESULT: &str = "show_result";

/// The tool list for MCP's `tools/list`. `show_result` exists only inside Scarlet,
/// and `query` only when values may be seen.
pub fn definitions(in_scarlet: bool, visibility: Visibility) -> Value {
    let sql = json!({"type": "object", "properties": {"sql": {"type": "string"}}, "required": ["sql"]});
    let query_note = match visibility {
        Visibility::Masked => " Personal data and secrets come back as stand-ins like ‹email 3›.",
        _ => "",
    };
    let mut tools = vec![
        json!({
            "name": "list_tables",
            "description": "Every table and view with its columns, types, foreign keys and approximate row count.",
            "inputSchema": {"type": "object", "properties": {}},
        }),
        json!({
            "name": "describe_table",
            "description": "One table in detail: columns, primary key, and links to and from other tables. \
                            Accepts `table` or `schema.table`.",
            "inputSchema": {"type": "object", "properties": {"table": {"type": "string"}}, "required": ["table"]},
        }),
    ];
    if visibility != Visibility::Schema {
        tools.push(json!({
            "name": "query",
            "description": format!(
                "Run one read-only SQL statement and get up to {QUERY_ROWS} rows back as tab-separated text. \
                 Use it to check what values look like before relying on them.{query_note}"
            ),
            "inputSchema": sql,
        }));
    }
    if in_scarlet {
        tools.push(json!({
            "name": SHOW_RESULT,
            "description": "Show the answer to the user: Scarlet runs this single read-only statement, shows its \
                            rows in a table and puts the SQL in the user's editor. Call it once, at the end.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sql": {"type": "string"},
                    "title": {"type": "string", "description": "A few words describing the rows, e.g. \"Repeat customers last month\"."},
                },
                "required": ["sql", "title"],
            },
        }));
    }
    Value::Array(tools)
}

/// A few words on what a tool call is doing, for the progress line.
pub fn activity(name: &str, arguments: &Value) -> String {
    match name {
        "list_tables" => "Reading the schema".into(),
        "describe_table" => format!("Looking at {}", arguments["table"].as_str().unwrap_or("a table")),
        "query" => "Trying a query".into(),
        SHOW_RESULT => "Running the answer".into(),
        _ => "Working".into(),
    }
}

/// Runs a tool other than `show_result`. The error side is shown to the agent too.
/// Values pass through `masker` unless `visibility` is `Full`.
pub async fn call(
    session: &Session,
    name: &str,
    arguments: &Value,
    visibility: Visibility,
    masker: &std::sync::Mutex<Masker>,
) -> Result<String, String> {
    match name {
        "list_tables" => overview(session).await,
        "describe_table" => {
            let table = arguments["table"].as_str().ok_or("`table` is required")?;
            describe_table(session, table).await
        }
        "query" if visibility != Visibility::Schema => {
            let sql = arguments["sql"].as_str().ok_or("`sql` is required")?;
            let outcome = session.peek(sql, QUERY_ROWS).await.map_err(|e| e.message)?;
            let mut masker = masker.lock().unwrap();
            Ok(rows_as_text(&outcome, QUERY_ROWS, (visibility == Visibility::Masked).then_some(&mut *masker)))
        }
        _ => Err(format!("Unknown tool {name}")),
    }
}

/// The whole schema, one line per relation:
/// `public.orders (table, ~20,000 rows): id bigint, customer_id integer → public.customers.id, …`
pub async fn overview(session: &Session) -> Result<String, String> {
    let (schemas, columns, links) = tokio::try_join!(session.catalog(), session.all_columns(), session.all_links())?;
    let mut out = String::new();
    let mut names = String::new();
    for relation in schemas.iter().flat_map(|s| &s.relations) {
        let key = (relation.schema.clone(), relation.name.clone());
        let head = relation_head(relation);
        let _ = writeln!(names, "{head}");
        let fields: Vec<String> = columns
            .get(&key)
            .into_iter()
            .flatten()
            .map(|(column, type_name)| {
                let target = links.iter().find(|(schema, table, link)| {
                    *schema == relation.schema && *table == relation.name && link.columns == [column.clone()]
                });
                match target {
                    Some((_, _, link)) => {
                        format!(
                            "{column} {type_name} → {}.{}.{}",
                            link.schema,
                            link.table,
                            link.other_columns.join(",")
                        )
                    }
                    None => format!("{column} {type_name}"),
                }
            })
            .collect();
        let _ = writeln!(out, "{head}: {}", fields.join(", "));
        // Composite foreign keys don't fit on a column.
        for (_, _, link) in links.iter().filter(|(schema, table, link)| {
            *schema == relation.schema && *table == relation.name && link.columns.len() > 1
        }) {
            let _ = writeln!(
                out,
                "  ({}) → {}.{}({})",
                link.columns.join(", "),
                link.schema,
                link.table,
                link.other_columns.join(", ")
            );
        }
    }
    if out.is_empty() {
        return Ok("The database has no tables or views.".into());
    }
    if out.len() > OVERVIEW_CHARS {
        return Ok(format!("{names}\nThe database is large, so columns are left out here; use describe_table."));
    }
    Ok(out)
}

fn relation_head(relation: &Relation) -> String {
    let kind = match relation.kind {
        RelationKind::Table => "table",
        RelationKind::View => "view",
        RelationKind::MaterializedView => "materialized view",
        RelationKind::ForeignTable => "foreign table",
    };
    match relation.estimate {
        Some(rows) => format!("{}.{} ({kind}, ~{rows} rows)", relation.schema, relation.name),
        None => format!("{}.{} ({kind})", relation.schema, relation.name),
    }
}

async fn describe_table(session: &Session, name: &str) -> Result<String, String> {
    let schemas = session.catalog().await?;
    let (schema, table) = match name.split_once('.') {
        Some((schema, table)) => (Some(schema.trim_matches('"')), table.trim_matches('"')),
        None => (None, name.trim_matches('"')),
    };
    let relation = schemas
        .iter()
        .flat_map(|s| &s.relations)
        .find(|r| r.name == table && schema.is_none_or(|s| r.schema == s))
        .ok_or_else(|| format!("No table or view named {name}"))?;
    let info = session.describe(relation).await?;
    let mut out = format!("{}\n", relation_head(relation));
    for column in &info.columns {
        let key = if column.primary_key { " primary key" } else { "" };
        let _ = writeln!(out, "  {} {}{key}", column.name, column.type_name);
    }
    for link in &info.links_out {
        let _ = writeln!(
            out,
            "links to {}.{}: ({}) → ({})",
            link.schema,
            link.table,
            link.columns.join(", "),
            link.other_columns.join(", ")
        );
    }
    for link in &info.links_in {
        let _ = writeln!(
            out,
            "linked from {}.{}: ({}) → ({})",
            link.schema,
            link.table,
            link.other_columns.join(", "),
            link.columns.join(", ")
        );
    }
    Ok(out)
}

/// Rows as tab-separated text with a header, values cut short, NULL spelled out.
/// With a masker, personal data is replaced: whole columns when the column (or
/// the table column behind it) is sensitive, and matches inside other values.
fn rows_as_text(outcome: &Outcome, limit: usize, mut masker: Option<&mut Masker>) -> String {
    let Some(columns) = &outcome.columns else {
        return format!("Done, {} rows affected.", outcome.affected);
    };
    let kinds: Vec<Option<&'static str>> = columns
        .iter()
        .enumerate()
        .map(|(i, column)| {
            let source = outcome.sources.get(i).cloned().flatten();
            sensitive_column(&column.name, &column.type_name)
                .or_else(|| sensitive_column(source.as_deref()?, &column.type_name))
        })
        .collect();
    let mut out = columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join("\t");
    out.push('\n');
    for row in outcome.rows.iter().take(limit) {
        let cells: Vec<String> = row
            .iter()
            .zip(&kinds)
            .map(|(value, kind)| match value {
                None => "NULL".into(),
                Some(text) => {
                    let text = match masker.as_deref_mut() {
                        Some(masker) => masker.cell(*kind, text),
                        None => text.clone(),
                    };
                    let flat = text.replace(['\t', '\n', '\r'], " ");
                    if flat.chars().count() > VALUE_CHARS {
                        format!("{}…", flat.chars().take(VALUE_CHARS).collect::<String>())
                    } else {
                        flat
                    }
                }
            })
            .collect();
        out.push_str(&cells.join("\t"));
        out.push('\n');
    }
    let shown = outcome.rows.len().min(limit);
    if outcome.truncated || outcome.rows.len() > limit {
        let _ = write!(out, "({shown} rows shown; there are more)");
    } else {
        let _ = write!(out, "({shown} {})", if shown == 1 { "row" } else { "rows" });
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::json;

    use super::*;
    use crate::connect::{Source, Target};

    /// Needs the fixture database (see `dev/seed.sql`), or a connection to it in `SCARLET_TEST_DATABASE`.
    #[tokio::test]
    #[ignore]
    async fn renamed_columns_stay_masked() {
        let database = std::env::var("SCARLET_TEST_DATABASE").unwrap_or_else(|_| "scarlet_dev".into());
        let target = Target::parse(&database, Source::Argument).unwrap();
        let session = Session::connect(&target, |_| {}).await.unwrap();
        let sql = json!({"sql": "SELECT email AS contact, name AS who, country FROM customers ORDER BY id LIMIT 3"});

        let full = call(&session, "query", &sql, Visibility::Full, &Mutex::default()).await.unwrap();
        assert!(full.contains("user1@example.com"), "{full}");

        let masked = call(&session, "query", &sql, Visibility::Masked, &Mutex::default()).await.unwrap();
        assert!(!masked.contains('@') && masked.contains("‹email 1›"), "{masked}");
        assert!(!full.lines().nth(1).unwrap().split('\t').nth(1).is_some_and(|name| masked.contains(name)));
        assert!(masked.contains("NO"), "ordinary columns stay readable: {masked}");

        assert!(call(&session, "query", &sql, Visibility::Schema, &Mutex::default()).await.is_err());
    }
}
