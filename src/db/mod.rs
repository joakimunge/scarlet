//! Database-neutral model shared by the UI and the drivers.

pub mod postgres;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelationKind {
    Table,
    View,
    MaterializedView,
    ForeignTable,
}

impl RelationKind {
    pub fn label(self) -> &'static str {
        match self {
            RelationKind::Table => "table",
            RelationKind::View => "view",
            RelationKind::MaterializedView => "mat view",
            RelationKind::ForeignTable => "foreign",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Relation {
    pub schema: String,
    pub name: String,
    pub kind: RelationKind,
    /// Planner row estimate; `None` when the relation has never been analyzed.
    pub estimate: Option<u64>,
}

impl Relation {
    pub fn same(&self, other: &Relation) -> bool {
        self.schema == other.schema && self.name == other.name
    }

    pub fn quoted(&self) -> String {
        format!("{}.{}", quote_ident(&self.schema), quote_ident(&self.name))
    }
}

#[derive(Clone, Debug)]
pub struct Schema {
    pub name: String,
    pub relations: Vec<Relation>,
}

/// How a column's values should be presented.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueKind {
    Text,
    Number,
    Bool,
    Json,
    Temporal,
}

#[derive(Clone, Debug)]
pub struct Column {
    pub name: String,
    pub type_name: String,
    pub kind: ValueKind,
    pub primary_key: bool,
}

/// One result row; values arrive in their text representation, `None` is SQL NULL.
pub type Row = Vec<Option<String>>;

pub struct TableInfo {
    pub columns: Vec<Column>,
    /// Columns that give rows a stable order for paging (the primary key, if any).
    pub order_by: Vec<String>,
    /// On-disk size of the relation's main fork, 0 for views.
    pub bytes: u64,
    /// Foreign keys from this relation to others.
    pub links_out: Vec<Link>,
    /// Foreign keys in other relations that point at this one.
    pub links_in: Vec<Link>,
}

/// A foreign key seen from one relation. For an outgoing link, `columns` here
/// reference `other_columns` in `schema.table`. For an incoming link, rows of
/// `schema.table` point here: their `other_columns` reference our `columns`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub schema: String,
    pub table: String,
    pub columns: Vec<String>,
    pub other_columns: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sort {
    pub column: String,
    pub descending: bool,
}

/// Which rows of a relation are shown, and in what order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct View {
    /// Plain words search whole rows; anything that looks like SQL is a WHERE condition.
    pub filter: String,
    pub sort: Option<Sort>,
}

pub enum Filter<'a> {
    All,
    Search(&'a str),
    Condition(&'a str),
}

impl View {
    pub fn filter(&self) -> Filter<'_> {
        let text = self.filter.trim();
        if text.is_empty() {
            Filter::All
        } else if looks_like_condition(text) {
            Filter::Condition(text)
        } else {
            Filter::Search(text)
        }
    }
}

/// Whether filter text is SQL rather than words to search for: it has a
/// comparison operator or an unmistakable SQL phrase outside quotes.
pub fn looks_like_condition(text: &str) -> bool {
    let bare = format!(" {} ", outside_quotes(&text.to_lowercase()));
    const PHRASES: &[&str] = &[
        " is null",
        " is not ",
        " is true",
        " is false",
        " is distinct ",
        " like '",
        " ilike '",
        " in (",
        " in(",
        " similar to '",
        " between ",
    ];
    bare.contains(['=', '<', '>', '~']) || PHRASES.iter().any(|p| bare.contains(p))
}

/// `text` with what is inside '...' and "..." removed, the quote marks kept
/// (an unclosed quote runs to the end).
pub fn outside_quotes(text: &str) -> String {
    let mut out = String::new();
    let mut quote = None;
    for c in text.chars() {
        match quote {
            Some(q) if c == q => {
                quote = None;
                out.push(c);
            }
            Some(_) => {}
            None => {
                if c == '\'' || c == '"' {
                    quote = Some(c);
                }
                out.push(c);
            }
        }
    }
    out
}

/// The name a relation's rows go by in generated SQL: the table's own name,
/// unless a column has that name (which would shadow the whole-row reference).
pub fn row_alias(relation: &Relation, columns: &[Column]) -> String {
    if columns.iter().any(|c| c.name == relation.name) { "scarlet_row".to_owned() } else { relation.name.clone() }
}

pub fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// An identifier as a person would write it: bare when that is unambiguous.
pub fn ident(name: &str) -> String {
    const RESERVED: &[&str] = &[
        "all",
        "and",
        "any",
        "array",
        "as",
        "asc",
        "both",
        "case",
        "check",
        "column",
        "constraint",
        "create",
        "default",
        "desc",
        "distinct",
        "do",
        "else",
        "end",
        "false",
        "for",
        "foreign",
        "from",
        "grant",
        "group",
        "having",
        "in",
        "into",
        "is",
        "join",
        "key",
        "limit",
        "not",
        "null",
        "offset",
        "on",
        "or",
        "order",
        "primary",
        "references",
        "select",
        "table",
        "then",
        "to",
        "true",
        "union",
        "unique",
        "user",
        "using",
        "when",
        "where",
        "with",
    ];
    let simple = name.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if simple && !RESERVED.contains(&name) { name.to_owned() } else { quote_ident(name) }
}

pub fn literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tells_searches_from_conditions() {
        for search in ["oslo", "O'Brien", "made in sweden", "rock and roll", "user1@example.com", "it's = fine'"] {
            assert!(!looks_like_condition(search), "{search}");
        }
        for condition in ["status = 'paid'", "total > 100", "note is null", "name ilike '%ek'", "id in (1, 2)"] {
            assert!(looks_like_condition(condition), "{condition}");
        }
    }

    #[test]
    fn writes_identifiers_like_a_person() {
        assert_eq!(ident("customer_id"), "customer_id");
        assert_eq!(ident("user"), "\"user\"");
        assert_eq!(ident("Weird Column"), "\"Weird Column\"");
    }
}
