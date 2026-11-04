//! PostgreSQL driver built on tokio-postgres.

use std::{
    collections::{HashMap, HashSet},
    error::Error as _,
    time::{Duration, Instant},
};

use futures::{StreamExt, pin_mut};

use native_tls::TlsConnector;
use postgres_native_tls::MakeTlsConnector;
use tokio::{sync::Mutex, task::JoinHandle};
use tokio_postgres::{
    CancelToken, Client, SimpleQueryMessage,
    error::{ErrorPosition, SqlState},
    types::{ToSql, Type},
};

use super::{
    Column, Filter, Link, Relation, RelationKind, Row, Schema, TableInfo, ValueKind, View, ident, literal,
    outside_quotes, quote_ident,
};
use crate::connect::Target;

#[derive(Debug)]
pub struct ConnectError {
    pub message: String,
    pub needs_password: bool,
}

#[derive(Debug)]
pub struct QueryError {
    pub message: String,
    /// The query was cancelled (by us, when the user moved on).
    pub canceled: bool,
}

/// An open connection pair: `client` serves everything the user is waiting on,
/// `aux` runs best-effort background work (row counts) under a statement timeout.
/// Both are read-only by default, so no filter can change data.
pub struct Session {
    client: Client,
    aux: Client,
    cancel: CancelToken,
    verify_tls: bool,
    target: Target,
    /// The SQL editor's own connection, opened on first use, so its queries
    /// (and its session settings) never get in the way of browsing.
    editor: Mutex<Option<Client>>,
    editor_cancel: std::sync::Mutex<Option<CancelToken>>,
    /// The agent's connection, opened on first use. Its queries always run in
    /// a read-only transaction that is rolled back.
    agent: Mutex<Option<Client>>,
    agent_cancel: std::sync::Mutex<Option<CancelToken>>,
}

/// What a statement from the editor produced.
pub struct Outcome {
    /// Present when the statement returns rows (even zero of them).
    pub columns: Option<Vec<Column>>,
    pub rows: Vec<Row>,
    /// Rows changed or returned, as the server reports it.
    pub affected: u64,
    /// More rows existed than were kept.
    pub truncated: bool,
    pub elapsed: Duration,
}

#[derive(Debug)]
pub struct RunError {
    pub message: String,
    /// The statement tried to change data in the read-only session.
    pub needs_write: bool,
    /// The user stopped it.
    pub canceled: bool,
}

/// Rows kept from one editor query; beyond this the query is stopped.
pub const RESULT_LIMIT: usize = 50_000;

const SCHEMAS_SQL: &str = "
SELECT nspname::text FROM pg_namespace
WHERE nspname <> 'information_schema' AND nspname NOT LIKE 'pg\\_%'
ORDER BY nspname <> 'public', nspname";

const RELATIONS_SQL: &str = "
SELECT n.nspname::text, c.relname::text, c.relkind::text, c.reltuples::float8
FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f') AND NOT c.relispartition
  AND n.nspname <> 'information_schema' AND n.nspname NOT LIKE 'pg\\_%'
ORDER BY lower(c.relname), c.relname";

const COLUMNS_SQL: &str = "
SELECT a.attname::text, format_type(a.atttypid, a.atttypmod), t.typcategory::text, t.typname::text
FROM pg_attribute a JOIN pg_type t ON t.oid = a.atttypid
WHERE a.attrelid = $1::text::regclass AND a.attnum > 0 AND NOT a.attisdropped
ORDER BY a.attnum";

const PRIMARY_KEY_SQL: &str = "
SELECT a.attname::text
FROM pg_index i
CROSS JOIN LATERAL unnest(i.indkey::int2[]) WITH ORDINALITY AS k(attnum, ord)
JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum
WHERE i.indrelid = $1::text::regclass AND i.indisprimary
ORDER BY k.ord";

/// Every column of every relation, for completing SQL.
const ALL_COLUMNS_SQL: &str = "
SELECT n.nspname::text, c.relname::text, a.attname::text, format_type(a.atttypid, a.atttypmod)
FROM pg_attribute a
JOIN pg_class c ON c.oid = a.attrelid JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f') AND a.attnum > 0 AND NOT a.attisdropped
  AND n.nspname <> 'information_schema' AND n.nspname NOT LIKE 'pg\\_%'
ORDER BY n.nspname, c.relname, a.attnum";

/// Foreign keys from and to a relation; a self-reference is both.
const LINKS_SQL: &str = "
SELECT c.conrelid = $1::text::regclass, c.confrelid = $1::text::regclass,
  sn.nspname::text, sc.relname::text, tn.nspname::text, tc.relname::text,
  ARRAY(SELECT a.attname::text FROM unnest(c.conkey) WITH ORDINALITY k(n, ord)
        JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k.n ORDER BY k.ord),
  ARRAY(SELECT a.attname::text FROM unnest(c.confkey) WITH ORDINALITY k(n, ord)
        JOIN pg_attribute a ON a.attrelid = c.confrelid AND a.attnum = k.n ORDER BY k.ord)
FROM pg_constraint c
JOIN pg_class sc ON sc.oid = c.conrelid JOIN pg_namespace sn ON sn.oid = sc.relnamespace
JOIN pg_class tc ON tc.oid = c.confrelid JOIN pg_namespace tn ON tn.oid = tc.relnamespace
WHERE c.contype = 'f' AND $1::text::regclass IN (c.conrelid, c.confrelid)
ORDER BY sc.relname, tc.relname, c.conname";

/// Every foreign key, as (schema, table, columns, referenced schema, table, columns).
const ALL_LINKS_SQL: &str = "
SELECT sn.nspname::text, sc.relname::text,
  ARRAY(SELECT a.attname::text FROM unnest(c.conkey) WITH ORDINALITY k(n, ord)
        JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k.n ORDER BY k.ord),
  tn.nspname::text, tc.relname::text,
  ARRAY(SELECT a.attname::text FROM unnest(c.confkey) WITH ORDINALITY k(n, ord)
        JOIN pg_attribute a ON a.attrelid = c.confrelid AND a.attnum = k.n ORDER BY k.ord)
FROM pg_constraint c
JOIN pg_class sc ON sc.oid = c.conrelid JOIN pg_namespace sn ON sn.oid = sc.relnamespace
JOIN pg_class tc ON tc.oid = c.confrelid JOIN pg_namespace tn ON tn.oid = tc.relnamespace
WHERE c.contype = 'f' AND sn.nspname <> 'information_schema' AND sn.nspname NOT LIKE 'pg\\_%'
ORDER BY sn.nspname, sc.relname, c.conname";

const DATABASES_SQL: &str = "
SELECT datname::text FROM pg_database
WHERE datallowconn AND NOT datistemplate AND has_database_privilege(datname, 'CONNECT')
ORDER BY datname = 'postgres', datname";

impl Session {
    /// Opens the session. `on_lost` runs if the main connection later drops with an error.
    pub async fn connect(
        target: &Target,
        on_lost: impl FnOnce(String) + Send + 'static,
    ) -> Result<Session, ConnectError> {
        let ((client, connection), (aux, _)) = tokio::try_join!(open(target), open(target))?;
        aux.batch_execute("SET statement_timeout = '5s'; SET default_transaction_read_only = on")
            .await
            .map_err(|e| connect_error(&e))?;
        client.batch_execute("SET default_transaction_read_only = on").await.map_err(|e| connect_error(&e))?;
        // Tables without a primary key are paged in physical order; synchronized
        // scans could start mid-table and make pages overlap. Best effort.
        let _ = client.batch_execute("SET synchronize_seqscans = off").await;
        let cancel = client.cancel_token();
        tokio::spawn(async move {
            if let Ok(Err(e)) = connection.await {
                on_lost(describe(&e));
            }
        });
        Ok(Session {
            client,
            aux,
            cancel,
            verify_tls: target.verify_tls,
            target: target.clone(),
            editor: Mutex::new(None),
            editor_cancel: std::sync::Mutex::new(None),
            agent: Mutex::new(None),
            agent_cancel: std::sync::Mutex::new(None),
        })
    }

    pub async fn catalog(&self) -> Result<Vec<Schema>, String> {
        let (schemas, relations) =
            tokio::try_join!(self.client.query(SCHEMAS_SQL, &[]), self.client.query(RELATIONS_SQL, &[]),)
                .map_err(|e| describe(&e))?;

        let mut out: Vec<Schema> =
            schemas.iter().map(|row| Schema { name: row.get(0), relations: Vec::new() }).collect();
        for row in relations {
            let schema: String = row.get(0);
            let Some(target) = out.iter_mut().find(|s| s.name == schema) else { continue };
            let kind = match row.get::<_, String>(2).as_str() {
                "v" => RelationKind::View,
                "m" => RelationKind::MaterializedView,
                "f" => RelationKind::ForeignTable,
                _ => RelationKind::Table,
            };
            let tuples: f64 = row.get(3);
            let estimate = (kind != RelationKind::View && tuples >= 0.0).then_some(tuples as u64);
            target.relations.push(Relation { schema, name: row.get(1), kind, estimate });
        }
        Ok(out)
    }

    /// Columns by (schema, table), each as (name, type), in table order.
    pub async fn all_columns(&self) -> Result<HashMap<(String, String), Vec<(String, String)>>, String> {
        let rows = self.client.query(ALL_COLUMNS_SQL, &[]).await.map_err(|e| describe(&e))?;
        let mut columns: HashMap<(String, String), Vec<(String, String)>> = HashMap::new();
        for row in rows {
            columns.entry((row.get(0), row.get(1))).or_default().push((row.get(2), row.get(3)));
        }
        Ok(columns)
    }

    /// Every foreign key in the database, for describing it to the agent.
    pub async fn all_links(&self) -> Result<Vec<(String, String, Link)>, String> {
        let rows = self.client.query(ALL_LINKS_SQL, &[]).await.map_err(|e| describe(&e))?;
        Ok(rows
            .iter()
            .map(|row| {
                let link =
                    Link { schema: row.get(3), table: row.get(4), columns: row.get(2), other_columns: row.get(5) };
                (row.get(0), row.get(1), link)
            })
            .collect())
    }

    pub async fn databases(&self) -> Result<Vec<String>, String> {
        let rows = self.client.query(DATABASES_SQL, &[]).await.map_err(|e| describe(&e))?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }

    pub async fn describe(&self, relation: &Relation) -> Result<TableInfo, String> {
        let name = relation.quoted();
        let params: [&(dyn ToSql + Sync); 1] = [&name];
        let (columns, key, size, links) = tokio::try_join!(
            self.client.query(COLUMNS_SQL, &params),
            self.client.query(PRIMARY_KEY_SQL, &params),
            self.client.query_one("SELECT pg_relation_size($1::text::regclass)", &params),
            self.client.query(LINKS_SQL, &params),
        )
        .map_err(|e| describe(&e))?;

        let (mut links_out, mut links_in) = (Vec::new(), Vec::new());
        for row in links {
            let (from_cols, to_cols): (Vec<String>, Vec<String>) = (row.get(6), row.get(7));
            if row.get::<_, bool>(0) {
                links_out.push(Link {
                    schema: row.get(4),
                    table: row.get(5),
                    columns: from_cols.clone(),
                    other_columns: to_cols.clone(),
                });
            }
            if row.get::<_, bool>(1) {
                links_in.push(Link {
                    schema: row.get(2),
                    table: row.get(3),
                    columns: to_cols,
                    other_columns: from_cols,
                });
            }
        }

        let order_by: Vec<String> = key.iter().map(|r| r.get(0)).collect();
        let key_set: HashSet<&str> = order_by.iter().map(String::as_str).collect();
        let columns = columns
            .iter()
            .map(|row| {
                let name: String = row.get(0);
                let category: String = row.get(2);
                let type_name: String = row.get(3);
                Column {
                    primary_key: key_set.contains(name.as_str()),
                    name,
                    type_name: row.get(1),
                    kind: value_kind(&category, &type_name),
                }
            })
            .collect();
        let bytes: i64 = size.get(0);
        Ok(TableInfo { columns, order_by, bytes: bytes.max(0) as u64, links_out, links_in })
    }

    /// Exact row count; may fail on the aux connection's statement timeout.
    pub async fn count(&self, relation: &Relation, alias: &str, view: &View) -> Result<u64, String> {
        let sql = format!(
            "SELECT count(*) FROM {} AS {}{}",
            relation.quoted(),
            quote_ident(alias),
            where_clause(alias, view)?
        );
        let row = self.aux.query_one(&sql, &[]).await.map_err(|e| describe(&e))?;
        Ok(row.get::<_, i64>(0).max(0) as u64)
    }

    /// One page of rows in their text form. Rows are ordered by the view's sort,
    /// then by `order_by` (the primary key), so pages never overlap.
    pub async fn page(
        &self,
        relation: &Relation,
        alias: &str,
        view: &View,
        order_by: &[String],
        offset: usize,
        limit: usize,
    ) -> Result<Vec<Row>, QueryError> {
        let filter = where_clause(alias, view).map_err(|message| QueryError { message, canceled: false })?;
        let mut order: Vec<String> = Vec::new();
        if let Some(sort) = &view.sort {
            order.push(format!("{} {}", quote_ident(&sort.column), if sort.descending { "DESC" } else { "ASC" }));
        }
        for column in order_by {
            if view.sort.as_ref().is_none_or(|s| &s.column != column) {
                order.push(quote_ident(column));
            }
        }
        let order = if order.is_empty() { String::new() } else { format!(" ORDER BY {}", order.join(", ")) };
        let alias = quote_ident(alias);
        let head = format!("SELECT {alias}.* FROM {} AS {alias}", relation.quoted());
        // Where the user's condition sits in the query, to explain errors in its terms.
        let condition = match view.filter() {
            Filter::Condition(text) => Some((head.chars().count() + " WHERE ".len(), text.chars().count())),
            _ => None,
        };
        let sql = format!("{head}{filter}{order} LIMIT {limit} OFFSET {offset}");
        let messages = self.client.simple_query(&sql).await.map_err(|e| query_error(&e, condition))?;
        Ok(messages
            .into_iter()
            .filter_map(|m| match m {
                SimpleQueryMessage::Row(row) => Some((0..row.len()).map(|i| row.get(i).map(str::to_owned)).collect()),
                _ => None,
            })
            .collect())
    }

    /// Runs one statement from the editor. Read-only unless `allow_writes`, in
    /// which case it runs in its own read-write transaction that commits on
    /// success. Rows stream in up to `RESULT_LIMIT`.
    pub async fn run(&self, sql: &str, allow_writes: bool) -> Result<Outcome, RunError> {
        let mut guard = self.editor.lock().await;
        if guard.is_none() {
            let (client, _) = open(&self.target).await.map_err(|e| RunError {
                message: e.message,
                needs_write: false,
                canceled: false,
            })?;
            client.batch_execute("SET default_transaction_read_only = on").await.map_err(|e| run_error(&e))?;
            *self.editor_cancel.lock().unwrap() = Some(client.cancel_token());
            *guard = Some(client);
        }
        let client = guard.as_ref().expect("opened above");
        let started = Instant::now();
        // Preparing first gives exact column types; the text protocol then gives every value as text.
        let statement = client.prepare(sql).await.map_err(|e| run_error(&e))?;
        let columns = (!statement.columns().is_empty()).then(|| {
            statement
                .columns()
                .iter()
                .map(|c| Column {
                    name: c.name().to_owned(),
                    type_name: sql_type_name(c.type_()),
                    kind: kind_of_type(c.type_()),
                    primary_key: false,
                })
                .collect::<Vec<_>>()
        });
        if allow_writes {
            client.batch_execute("BEGIN READ WRITE").await.map_err(|e| run_error(&e))?;
        }
        // Taken out first: the lock must not be held across the query.
        let cancel = self.editor_cancel.lock().unwrap().clone();
        let result = collect(client, sql, RESULT_LIMIT, cancel, self.verify_tls).await;
        if allow_writes {
            let end = if result.is_ok() { "COMMIT" } else { "ROLLBACK" };
            client.batch_execute(end).await.map_err(|e| run_error(&e))?;
        }
        let (rows, affected, truncated) = result?;
        Ok(Outcome { columns, rows, affected, truncated, elapsed: started.elapsed() })
    }

    /// Runs one statement for the agent: a single statement only (preparing
    /// it rejects several), inside a read-only transaction that is always
    /// rolled back, so not even session settings survive it. Keeps up to `limit` rows.
    pub async fn peek(&self, sql: &str, limit: usize) -> Result<Outcome, RunError> {
        let mut guard = self.agent.lock().await;
        if guard.is_none() {
            let (client, _) = open(&self.target).await.map_err(|e| RunError {
                message: e.message,
                needs_write: false,
                canceled: false,
            })?;
            client
                .batch_execute("SET default_transaction_read_only = on; SET statement_timeout = '30s'")
                .await
                .map_err(|e| run_error(&e))?;
            *self.agent_cancel.lock().unwrap() = Some(client.cancel_token());
            *guard = Some(client);
        }
        let client = guard.as_ref().expect("opened above");
        let started = Instant::now();
        let statement = client.prepare(sql).await.map_err(|e| run_error(&e))?;
        let columns = (!statement.columns().is_empty()).then(|| {
            statement
                .columns()
                .iter()
                .map(|c| Column {
                    name: c.name().to_owned(),
                    type_name: sql_type_name(c.type_()),
                    kind: kind_of_type(c.type_()),
                    primary_key: false,
                })
                .collect::<Vec<_>>()
        });
        client.batch_execute("BEGIN READ ONLY").await.map_err(|e| run_error(&e))?;
        let cancel = self.agent_cancel.lock().unwrap().clone();
        let result = collect(client, sql, limit, cancel, self.verify_tls).await;
        client.batch_execute("ROLLBACK").await.map_err(|e| run_error(&e))?;
        let (rows, affected, truncated) = result?;
        Ok(Outcome { columns, rows, affected, truncated, elapsed: started.elapsed() })
    }

    /// Stops the editor's running query, if any.
    pub async fn cancel_editor(&self) {
        let token = self.editor_cancel.lock().unwrap().clone();
        if let (Some(token), Ok(tls)) = (token, tls(self.verify_tls)) {
            let _ = token.cancel_query(tls).await;
        }
    }

    /// Asks the server to stop whatever the main connection is running.
    pub async fn cancel(&self) {
        if let Ok(tls) = tls(self.verify_tls) {
            let _ = self.cancel.cancel_query(tls).await;
        }
    }
}

/// Streams a statement's rows, stopping the query once `limit` rows are in.
async fn collect(
    client: &Client,
    sql: &str,
    limit: usize,
    cancel: Option<CancelToken>,
    verify_tls: bool,
) -> Result<(Vec<Row>, u64, bool), RunError> {
    let stream = client.simple_query_raw(sql).await.map_err(|e| run_error(&e))?;
    pin_mut!(stream);
    let (mut rows, mut affected, mut truncated) = (Vec::new(), 0, false);
    while let Some(message) = stream.next().await {
        match message {
            Ok(SimpleQueryMessage::Row(row)) => {
                if rows.len() == limit {
                    truncated = true;
                    if let (Some(token), Ok(tls)) = (&cancel, tls(verify_tls)) {
                        let _ = token.cancel_query(tls).await;
                    }
                    break;
                }
                rows.push((0..row.len()).map(|i| row.get(i).map(str::to_owned)).collect());
            }
            Ok(SimpleQueryMessage::CommandComplete(n)) => affected = n,
            Ok(_) => {}
            Err(e) => return Err(run_error(&e)),
        }
    }
    // Let the cancelled query finish failing before the connection is used again.
    if truncated {
        while stream.next().await.is_some() {}
    }
    Ok((rows, affected, truncated))
}

fn run_error(e: &tokio_postgres::Error) -> RunError {
    let mut message = describe(e);
    if let Some(db) = e.as_db_error()
        && let Some(ErrorPosition::Original(position)) = db.position()
    {
        message.push_str(&format!(" (at character {position})"));
    }
    RunError {
        message,
        needs_write: e.code() == Some(&SqlState::READ_ONLY_SQL_TRANSACTION),
        canceled: e.code() == Some(&SqlState::QUERY_CANCELED),
    }
}

/// The type as `format_type` spells it for table columns, so results and
/// tables use the same names (`bigint`, not `int8`).
fn sql_type_name(ty: &Type) -> String {
    let name = match *ty {
        Type::INT2 => "smallint",
        Type::INT4 => "integer",
        Type::INT8 => "bigint",
        Type::FLOAT4 => "real",
        Type::FLOAT8 => "double precision",
        Type::BOOL => "boolean",
        Type::VARCHAR => "character varying",
        Type::BPCHAR => "character",
        Type::TIMESTAMP => "timestamp without time zone",
        Type::TIMESTAMPTZ => "timestamp with time zone",
        Type::TIME => "time without time zone",
        Type::TIMETZ => "time with time zone",
        Type::INT2_ARRAY => "smallint[]",
        Type::INT4_ARRAY => "integer[]",
        Type::INT8_ARRAY => "bigint[]",
        Type::TEXT_ARRAY => "text[]",
        Type::VARCHAR_ARRAY => "character varying[]",
        _ => return ty.name().strip_prefix('_').map_or_else(|| ty.name().to_owned(), |inner| format!("{inner}[]")),
    };
    name.to_owned()
}

fn kind_of_type(ty: &Type) -> ValueKind {
    match *ty {
        Type::INT2
        | Type::INT4
        | Type::INT8
        | Type::FLOAT4
        | Type::FLOAT8
        | Type::NUMERIC
        | Type::OID
        | Type::MONEY => ValueKind::Number,
        Type::BOOL => ValueKind::Bool,
        Type::JSON | Type::JSONB => ValueKind::Json,
        Type::DATE | Type::TIME | Type::TIMETZ | Type::TIMESTAMP | Type::TIMESTAMPTZ | Type::INTERVAL => {
            ValueKind::Temporal
        }
        _ => ValueKind::Text,
    }
}

/// A query error, described relative to the user's filter condition when the
/// server points at a position: past its end means it is unfinished.
fn query_error(e: &tokio_postgres::Error, condition: Option<(usize, usize)>) -> QueryError {
    let canceled = e.code() == Some(&SqlState::QUERY_CANCELED);
    let mut message = describe(e);
    if let (Some((start, len)), Some(db)) = (condition, e.as_db_error())
        && let Some(ErrorPosition::Original(position)) = db.position()
    {
        let position = *position as usize;
        if position > start + len && db.code() == &SqlState::SYNTAX_ERROR {
            message = "The filter looks unfinished".into();
        } else if position > start && position <= start + len {
            message.push_str(&format!(" (at character {} of the filter)", position - start));
        }
    }
    QueryError { message, canceled }
}

fn where_clause(alias: &str, view: &View) -> Result<String, String> {
    Ok(match view.filter() {
        Filter::All => String::new(),
        Filter::Search(words) => format!(" WHERE {}", search_condition(alias, words)),
        Filter::Condition(sql) => {
            if outside_quotes(sql).contains(';') {
                return Err("A filter is a single condition, so it can't contain ;".into());
            }
            format!(" WHERE {sql}")
        }
    })
}

/// Rows whose text form contains `words`, case-insensitively.
pub fn search_condition(alias: &str, words: &str) -> String {
    let escaped = words.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
    format!("{}::text ILIKE {}", ident(alias), literal(&format!("%{escaped}%")))
}

/// A condition matching `value` in `name`, typed after `column`. With `exclude`
/// it matches everything else, NULLs included.
pub fn value_condition(name: &str, column: &Column, value: Option<&str>, exclude: bool) -> String {
    let lhs = ident(name);
    let Some(value) = value else {
        return format!("{lhs} IS {}NULL", if exclude { "NOT " } else { "" });
    };
    let plain_number = !value.is_empty() && value.chars().all(|c| c.is_ascii_digit() || "+-.eE".contains(c));
    let (lhs, rhs) = match column.kind {
        ValueKind::Number if plain_number => (lhs, value.to_owned()),
        ValueKind::Bool => (lhs, if value == "t" { "true" } else { "false" }.to_owned()),
        // json (unlike jsonb) has no equality operator.
        ValueKind::Json if column.type_name == "json" => (format!("{lhs}::text"), literal(value)),
        _ => (lhs, literal(value)),
    };
    format!("{lhs} {} {rhs}", if exclude { "IS DISTINCT FROM" } else { "=" })
}

/// Lists the databases on a server with a short-lived connection.
pub async fn list_databases(target: &Target) -> Result<Vec<String>, ConnectError> {
    let (client, _) = open(target).await?;
    let rows = client.query(DATABASES_SQL, &[]).await.map_err(|e| connect_error(&e))?;
    Ok(rows.iter().map(|r| r.get(0)).collect())
}

type Connection = JoinHandle<Result<(), tokio_postgres::Error>>;

/// Like libpq, only verify-ca / verify-full check certificates; prefer and require just encrypt.
fn tls(verify: bool) -> Result<MakeTlsConnector, String> {
    let connector = TlsConnector::builder()
        .danger_accept_invalid_certs(!verify)
        .danger_accept_invalid_hostnames(!verify)
        .build()
        .map_err(|e| e.to_string())?;
    Ok(MakeTlsConnector::new(connector))
}

async fn open(target: &Target) -> Result<(Client, Connection), ConnectError> {
    let tls = tls(target.verify_tls).map_err(|message| ConnectError { message, needs_password: false })?;
    let (client, connection) = target.config.connect(tls).await.map_err(|e| connect_error(&e))?;
    Ok((client, tokio::spawn(connection)))
}

fn value_kind(category: &str, type_name: &str) -> ValueKind {
    match (category, type_name) {
        (_, "json" | "jsonb") => ValueKind::Json,
        ("N", _) => ValueKind::Number,
        ("B", _) => ValueKind::Bool,
        ("D", _) => ValueKind::Temporal,
        _ => ValueKind::Text,
    }
}

fn connect_error(e: &tokio_postgres::Error) -> ConnectError {
    let message = describe(e);
    let needs_password = e.code() == Some(&SqlState::INVALID_PASSWORD) || message.contains("password missing");
    ConnectError { message, needs_password }
}

/// A one-line human description of a driver error, including server detail and hint.
pub fn describe(e: &tokio_postgres::Error) -> String {
    if let Some(db) = e.as_db_error() {
        let mut text = db.message().to_owned();
        if let Some(detail) = db.detail() {
            text.push_str(". ");
            text.push_str(detail);
        }
        if let Some(hint) = db.hint() {
            text.push_str(". ");
            text.push_str(hint);
        }
        return text;
    }
    let mut text = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        let part = cause.to_string();
        if !text.contains(&part) {
            text.push_str(": ");
            text.push_str(&part);
        }
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs the fixture database: `createdb scarlet_dev && psql -d scarlet_dev -f dev/seed.sql`.
    #[tokio::test]
    #[ignore]
    async fn agent_queries_cannot_change_anything() {
        let target = Target::parse("scarlet_dev", crate::connect::Source::Argument).unwrap();
        let session = Session::connect(&target, |_| {}).await.unwrap();
        let count = "SELECT count(*) FROM orders";
        let before = session.peek(count, 1).await.unwrap().rows;

        assert!(session.peek("SELECT 1; DELETE FROM orders", 10).await.is_err());
        assert!(session.peek("WITH gone AS (DELETE FROM orders RETURNING 1) SELECT 1", 10).await.is_err());
        // A statement that switches writes back on is undone with its transaction.
        session.peek("SELECT set_config('default_transaction_read_only', 'off', false)", 1).await.unwrap();
        assert!(session.peek("WITH gone AS (DELETE FROM orders RETURNING 1) SELECT 1", 10).await.is_err());

        assert_eq!(session.peek(count, 1).await.unwrap().rows, before);
    }

    fn column(kind: ValueKind, type_name: &str) -> Column {
        Column { name: "c".into(), type_name: type_name.into(), kind, primary_key: false }
    }

    #[test]
    fn writes_value_conditions_that_match_exactly() {
        let number = column(ValueKind::Number, "bigint");
        assert_eq!(value_condition("customer_id", &number, Some("42"), false), "customer_id = 42");
        assert_eq!(value_condition("note", &number, None, true), "note IS NOT NULL");
        let text = column(ValueKind::Text, "text");
        assert_eq!(value_condition("name", &text, Some("O'Brien"), true), "name IS DISTINCT FROM 'O''Brien'");
        assert_eq!(value_condition("vip", &column(ValueKind::Bool, "boolean"), Some("t"), false), "vip = true");
        assert_eq!(value_condition("doc", &column(ValueKind::Json, "json"), Some("{}"), false), "doc::text = '{}'");
    }

    #[test]
    fn escapes_searches() {
        assert_eq!(search_condition("customers", "50%_off"), "customers::text ILIKE '%50\\%\\_off%'");
    }
}
