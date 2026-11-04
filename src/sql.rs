//! SQL text analysis for the editor: tokens for highlighting, statement boundaries for running the
//! statement under the cursor, and what to complete where the cursor is. Pure functions over `&str` that
//! accept any half-typed text, since they run on every keystroke. All offsets are bytes on char boundaries.

use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    Keyword,
    Identifier,
    QuotedIdentifier,
    String,
    Number,
    Comment,
    Operator,
    Punctuation,
    Parameter,
    Whitespace,
}

impl TokenKind {
    /// Whether the token carries meaning, as opposed to whitespace and comments.
    fn significant(self) -> bool {
        !matches!(self, TokenKind::Whitespace | TokenKind::Comment)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub start: usize,
    pub end: usize,
}

/// Uppercase and sorted (byte order) so `is_keyword` can binary search.
pub const KEYWORDS: &[&str] = &[
    "ADD",
    "ALL",
    "ALTER",
    "ALWAYS",
    "ANALYZE",
    "AND",
    "ANY",
    "ARRAY",
    "ARRAY_AGG",
    "AS",
    "ASC",
    "AT",
    "ATOMIC",
    "AVG",
    "BEGIN",
    "BETWEEN",
    "BIGINT",
    "BIGSERIAL",
    "BOOL",
    "BOOLEAN",
    "BOTH",
    "BY",
    "BYTEA",
    "CASCADE",
    "CASE",
    "CAST",
    "CHAR",
    "CHECK",
    "COALESCE",
    "COLLATE",
    "COLUMN",
    "COMMIT",
    "CONCAT",
    "CONCURRENTLY",
    "CONFLICT",
    "CONSTRAINT",
    "COPY",
    "COUNT",
    "CREATE",
    "CROSS",
    "CURRENT",
    "CURRENT_DATE",
    "CURRENT_TIMESTAMP",
    "CURRENT_USER",
    "DATABASE",
    "DATE",
    "DATE_TRUNC",
    "DECIMAL",
    "DEFAULT",
    "DEFERRABLE",
    "DELETE",
    "DESC",
    "DISTINCT",
    "DO",
    "DOUBLE",
    "DROP",
    "ELSE",
    "END",
    "ESCAPE",
    "EXCEPT",
    "EXCLUDE",
    "EXECUTE",
    "EXISTS",
    "EXPLAIN",
    "EXTENSION",
    "EXTRACT",
    "FALSE",
    "FETCH",
    "FILTER",
    "FIRST",
    "FLOAT",
    "FOLLOWING",
    "FOR",
    "FOREIGN",
    "FROM",
    "FULL",
    "FUNCTION",
    "GENERATED",
    "GENERATE_SERIES",
    "GEN_RANDOM_UUID",
    "GRANT",
    "GREATEST",
    "GROUP",
    "HAVING",
    "IF",
    "ILIKE",
    "IN",
    "INDEX",
    "INHERITS",
    "INNER",
    "INSERT",
    "INT",
    "INTEGER",
    "INTERSECT",
    "INTERVAL",
    "INTO",
    "IS",
    "ISNULL",
    "JOIN",
    "JSON",
    "JSONB",
    "JSONB_AGG",
    "JSON_AGG",
    "LANGUAGE",
    "LAST",
    "LATERAL",
    "LEADING",
    "LEAST",
    "LEFT",
    "LENGTH",
    "LIKE",
    "LIMIT",
    "LOCK",
    "LOWER",
    "MATCHED",
    "MATERIALIZED",
    "MAX",
    "MERGE",
    "MIN",
    "NATURAL",
    "NOT",
    "NOTHING",
    "NOTNULL",
    "NOW",
    "NULL",
    "NULLIF",
    "NULLS",
    "NUMERIC",
    "OF",
    "OFFSET",
    "ON",
    "ONLY",
    "OR",
    "ORDER",
    "OUTER",
    "OVER",
    "OVERLAY",
    "OVERRIDING",
    "PARTITION",
    "POSITION",
    "PRECEDING",
    "PRECISION",
    "PRIMARY",
    "PROCEDURE",
    "RANK",
    "REAL",
    "RECURSIVE",
    "REFERENCES",
    "REFRESH",
    "RELEASE",
    "RENAME",
    "REPLACE",
    "RESTRICT",
    "RETURNING",
    "RETURNS",
    "REVOKE",
    "RIGHT",
    "ROLLBACK",
    "ROUND",
    "ROW",
    "ROWS",
    "ROW_NUMBER",
    "SAVEPOINT",
    "SCHEMA",
    "SELECT",
    "SEQUENCE",
    "SERIAL",
    "SET",
    "SHOW",
    "SIMILAR",
    "SMALLINT",
    "SOME",
    "START",
    "STRING_AGG",
    "SUBSTRING",
    "SUM",
    "TABLE",
    "TABLESAMPLE",
    "TEMP",
    "TEMPORARY",
    "TEXT",
    "THEN",
    "TIES",
    "TIME",
    "TIMESTAMP",
    "TIMESTAMPTZ",
    "TO",
    "TO_CHAR",
    "TRAILING",
    "TRANSACTION",
    "TRIGGER",
    "TRIM",
    "TRUE",
    "TRUNCATE",
    "UNBOUNDED",
    "UNION",
    "UNIQUE",
    "UNLOGGED",
    "UNNEST",
    "UPDATE",
    "UPPER",
    "USING",
    "UUID",
    "VACUUM",
    "VALUES",
    "VARCHAR",
    "VARYING",
    "VIEW",
    "WHEN",
    "WHERE",
    "WINDOW",
    "WITH",
    "WITHIN",
    "WITHOUT",
    "ZONE",
];

pub fn is_keyword(word: &str) -> bool {
    KEYWORDS.binary_search_by(|k| k.bytes().cmp(word.bytes().map(|b| b.to_ascii_uppercase()))).is_ok()
}

/// Never fails; unterminated strings, comments and quoted identifiers run to the end of the text.
pub fn tokenize(text: &str) -> Vec<Token> {
    lex(text).tokens
}

/// Byte ranges of the statements in `text`, split on `;` outside strings, comments, quoted identifiers and
/// dollar-quoted bodies. Ranges exclude the `;` and the whitespace and comments around a statement;
/// statements with nothing else in them are omitted.
pub fn statements(text: &str) -> Vec<Range<usize>> {
    let tokens = lex(text).tokens;
    chunks(text, &tokens).iter().filter_map(|chunk| trimmed(&tokens[chunk.tokens.clone()])).collect()
}

/// The statement the cursor belongs to: the one containing it (its end included). Between statements, the
/// previous one if it ends on the cursor's line, else the next one if it starts on that line, else the
/// previous one, else the next one.
pub fn statement_at(text: &str, cursor: usize) -> Option<Range<usize>> {
    let all = statements(text);
    let cursor = cursor.min(text.len());
    let next = all.partition_point(|s| s.end < cursor);
    if let Some(s) = all.get(next)
        && s.start <= cursor
    {
        return Some(s.clone());
    }
    let one_line = |from: usize, to: usize| !text.as_bytes()[from..to].contains(&b'\n');
    let before = next.checked_sub(1).map(|i| all[i].clone());
    let after = all.get(next).cloned();
    if let Some(s) = &before
        && one_line(s.end, cursor)
    {
        return before;
    }
    if let Some(s) = &after
        && one_line(cursor, s.start)
    {
        return after;
    }
    before.or(after)
}

/// A table named in a statement. Names follow Postgres rules: unquoted ones lowercased, quoted ones exact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableRef {
    pub schema: Option<String>,
    pub name: String,
    pub alias: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expect {
    Table,
    Column,
    Any,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completion {
    /// The partial word being typed (may be empty), and the byte offset where it starts.
    pub prefix: String,
    pub start: usize,
    /// Text before a dot right before the prefix (`o` in `o.cu|`, `public` in `public.ord|`), unquoted.
    pub qualifier: Option<String>,
    pub expect: Expect,
    /// Tables referenced in the current statement (FROM, JOIN, UPDATE, INSERT INTO, DELETE FROM, comma lists,
    /// CTE names), not counting the one being typed at the cursor.
    pub tables: Vec<TableRef>,
}

/// What to suggest at `cursor`. None inside a string, comment or quoted identifier, and right after a number
/// or parameter.
pub fn completion_context(text: &str, cursor: usize) -> Option<Completion> {
    let mut cursor = cursor.min(text.len());
    while !text.is_char_boundary(cursor) {
        cursor -= 1;
    }
    let lexed = lex(text);
    let tokens = &lexed.tokens;

    // The token that ends at or runs past the cursor; there is none at offset 0.
    let at = tokens.partition_point(|t| t.end < cursor);
    let mut start = cursor;
    // tokens[..before] lie wholly before the word being completed.
    let mut before = at;
    if cursor > 0
        && let Some(t) = tokens.get(at)
    {
        let inside = cursor < t.end || lexed.is_open(at);
        match t.kind {
            TokenKind::String | TokenKind::QuotedIdentifier if inside => return None,
            // A line comment runs to the end of its line, so the cursor there is still inside it.
            TokenKind::Comment if inside || text[t.start..].starts_with("--") => return None,
            TokenKind::Number | TokenKind::Parameter => return None,
            TokenKind::Keyword | TokenKind::Identifier => start = t.start,
            _ => before = at + 1,
        }
    }

    // `qualifier.` right before the word.
    let mut context_end = before;
    let mut qualifier = None;
    let mut word_start = start;
    if before > 0 && is_punct(text, tokens[before - 1], b'.') && tokens[before - 1].end == start {
        let dot = tokens[before - 1];
        context_end = before - 1;
        if before > 1 && is_name_kind(tokens[before - 2].kind) && tokens[before - 2].end == dot.start {
            qualifier = Some(ident_name(text, &lexed, before - 2));
            context_end = before - 2;
            word_start = tokens[before - 2].start;
        }
    }

    let chunks = chunks(text, tokens);
    let chunk = chunks.iter().find(|c| c.span.start <= cursor && cursor <= c.span.end)?;
    let significant: Vec<usize> = chunk.tokens.clone().filter(|&k| tokens[k].kind.significant()).collect();
    let context_len = significant.partition_point(|&k| k < context_end);

    let mut tables: Vec<TableRef> = Vec::new();
    for (table, span) in table_refs(text, &lexed, &significant) {
        // Skip the name being typed: in `from ord|` or `from public.|` it is not a table yet.
        let typing = span.start <= cursor && span.end >= word_start;
        if !typing && !tables.contains(&table) {
            tables.push(table);
        }
    }

    let context = expect_after(text, tokens, &significant[..context_len]);
    let expect = match &qualifier {
        None => context,
        Some(_) if context == Expect::Table => Expect::Table,
        Some(q) if tables.iter().any(|t| t.alias.as_ref() == Some(q) || &t.name == q) => Expect::Column,
        Some(_) => context,
    };
    Some(Completion { prefix: text[start..cursor].to_owned(), start, qualifier, expect, tables })
}

// Lexing

struct Lexed {
    tokens: Vec<Token>,
    /// The last token is a string, quoted identifier or block comment that runs off the end of the text.
    open: bool,
}

impl Lexed {
    fn is_open(&self, index: usize) -> bool {
        self.open && index + 1 == self.tokens.len()
    }
}

/// Every decision is made on ASCII bytes, and non-ASCII bytes only ever continue an identifier, string or
/// comment, so token boundaries always fall on char boundaries.
fn lex(text: &str) -> Lexed {
    let b = text.as_bytes();
    let at = |j: usize| b.get(j).copied().unwrap_or(0);
    let mut tokens: Vec<Token> = Vec::new();
    let mut open = false;
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let c = b[i];
        let kind = if is_space(c) {
            i = skip(b, i, is_space);
            TokenKind::Whitespace
        } else if c == b'-' && at(i + 1) == b'-' {
            i = b[i..].iter().position(|&x| x == b'\n').map_or(b.len(), |p| i + p);
            TokenKind::Comment
        } else if c == b'/' && at(i + 1) == b'*' {
            (i, open) = block_comment(b, i);
            TokenKind::Comment
        } else if c == b'\'' {
            (i, open) = quoted(b, i, false);
            TokenKind::String
        } else if c == b'"' {
            (i, open) = quoted(b, i, false);
            TokenKind::QuotedIdentifier
        } else if c == b'$' && at(i + 1).is_ascii_digit() {
            i = skip(b, i + 1, |x| x.is_ascii_digit());
            TokenKind::Parameter
        } else if c == b'$' {
            match dollar_tag(b, i) {
                Some(tag_end) => {
                    let tag = &text[i..tag_end];
                    match text[tag_end..].find(tag) {
                        Some(p) => i = tag_end + p + tag.len(),
                        None => (i, open) = (b.len(), true),
                    }
                    TokenKind::String
                }
                None => {
                    i += 1;
                    TokenKind::Punctuation
                }
            }
        } else if c.is_ascii_digit() || (c == b'.' && at(i + 1).is_ascii_digit()) {
            i = number(b, i);
            TokenKind::Number
        } else if is_ident_start(c) {
            let next = at(i + 1);
            if next == b'\'' && matches!(c, b'e' | b'E') {
                (i, open) = quoted(b, i + 1, true);
                TokenKind::String
            } else if next == b'\'' && matches!(c, b'b' | b'B' | b'x' | b'X' | b'n' | b'N') {
                (i, open) = quoted(b, i + 1, false);
                TokenKind::String
            } else if matches!(c, b'u' | b'U') && next == b'&' && matches!(at(i + 2), b'\'' | b'"') {
                let string = at(i + 2) == b'\'';
                (i, open) = quoted(b, i + 2, false);
                if string { TokenKind::String } else { TokenKind::QuotedIdentifier }
            } else {
                i = skip(b, i, is_ident_cont);
                // A word right after a dot is a column or table name, whatever it spells.
                let after_dot = tokens.last().is_some_and(|t| is_punct(text, *t, b'.'));
                if !after_dot && is_keyword(&text[start..i]) { TokenKind::Keyword } else { TokenKind::Identifier }
            }
        } else if c == b':' {
            if matches!(at(i + 1), b':' | b'=') {
                i += 2;
                TokenKind::Operator
            } else {
                i += 1;
                TokenKind::Punctuation
            }
        } else if is_operator(c) {
            i += 1;
            // Like Postgres, an operator stops where a comment starts: `a+--x` is `a`, `+` and a comment.
            while i < b.len() && is_operator(b[i]) && !matches!((b[i], at(i + 1)), (b'-', b'-') | (b'/', b'*')) {
                i += 1;
            }
            TokenKind::Operator
        } else {
            // ( ) [ ] , ; . and any other ASCII character.
            i += 1;
            TokenKind::Punctuation
        };
        tokens.push(Token { kind, start, end: i });
    }
    Lexed { tokens, open }
}

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// Postgres lets any non-ASCII character into identifiers.
fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c >= 0x80
}

fn is_ident_cont(c: u8) -> bool {
    is_ident_start(c) || c.is_ascii_digit() || c == b'$'
}

fn is_operator(c: u8) -> bool {
    b"+-*/<>=~!@#%^&|`?".contains(&c)
}

fn skip(b: &[u8], mut i: usize, keep: impl Fn(u8) -> bool) -> usize {
    while i < b.len() && keep(b[i]) {
        i += 1;
    }
    i
}

/// End of a string or quoted identifier opening at `i`, where a doubled quote is an escaped one; and whether
/// it runs off the end. `backslash` is for E'..' strings, where `\` escapes the next character.
fn quoted(b: &[u8], i: usize, backslash: bool) -> (usize, bool) {
    let quote = b[i];
    let mut j = i + 1;
    while j < b.len() {
        let escaped = b.get(j + 1) == Some(&quote);
        if (backslash && b[j] == b'\\') || (b[j] == quote && escaped) {
            j += 2;
        } else if b[j] == quote {
            return (j + 1, false);
        } else {
            j += 1;
        }
    }
    (b.len(), true)
}

/// Block comments nest in Postgres.
fn block_comment(b: &[u8], i: usize) -> (usize, bool) {
    let mut depth = 0;
    let mut j = i;
    while j < b.len() {
        match (b[j], b.get(j + 1)) {
            (b'/', Some(b'*')) => {
                depth += 1;
                j += 2;
            }
            (b'*', Some(b'/')) => {
                depth -= 1;
                j += 2;
                if depth == 0 {
                    return (j, false);
                }
            }
            _ => j += 1,
        }
    }
    (b.len(), true)
}

/// The end of a dollar-quote opening tag (`$$`, `$body$`) starting at `i`.
fn dollar_tag(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    if b.get(j).is_some_and(|&c| is_ident_start(c)) {
        j = skip(b, j, |c| c != b'$' && is_ident_cont(c));
    }
    (b.get(j) == Some(&b'$')).then_some(j + 1)
}

/// Decimal (with `_` separators, fraction and exponent) or 0x / 0o / 0b integers.
fn number(b: &[u8], mut i: usize) -> usize {
    let at = |j: usize| b.get(j).copied().unwrap_or(0);
    let digit = |c: u8| c.is_ascii_digit() || c == b'_';
    if b[i] == b'0' && matches!(at(i + 1), b'x' | b'X' | b'o' | b'O' | b'b' | b'B') && at(i + 2).is_ascii_hexdigit() {
        return skip(b, i + 2, |c| c.is_ascii_hexdigit() || c == b'_');
    }
    i = skip(b, i, digit);
    if at(i) == b'.' && at(i + 1) != b'.' {
        i = skip(b, i + 1, digit);
    }
    if matches!(at(i), b'e' | b'E') {
        let j = if matches!(at(i + 1), b'+' | b'-') { i + 2 } else { i + 1 };
        if at(j).is_ascii_digit() {
            i = skip(b, j, digit);
        }
    }
    i
}

// Statements

/// The text between two semicolons: its tokens (by index) and bytes, without the `;`.
struct Chunk {
    tokens: Range<usize>,
    span: Range<usize>,
}

/// Splits on `;`, except inside the `BEGIN ATOMIC ... END` body of a CREATE FUNCTION or PROCEDURE,
/// which holds whole statements of its own (psql makes the same exception).
fn chunks(text: &str, tokens: &[Token]) -> Vec<Chunk> {
    let mut out = Vec::new();
    let (mut first_token, mut first_byte) = (0, 0);
    let (mut words, mut create, mut routine, mut begin, mut depth) = (0, false, false, false, 0u32);
    for (k, &t) in tokens.iter().enumerate() {
        match t.kind {
            TokenKind::Whitespace | TokenKind::Comment => {}
            TokenKind::Keyword | TokenKind::Identifier => {
                let is = |word: &str| text[t.start..t.end].eq_ignore_ascii_case(word);
                if words == 0 {
                    create = is("create");
                }
                words += 1;
                if begin && is("atomic") {
                    depth = 1;
                } else if depth > 0 {
                    if is("case") {
                        depth += 1;
                    } else if is("end") {
                        depth -= 1;
                    }
                } else if create && (is("function") || is("procedure")) {
                    routine = true;
                }
                begin = routine && depth == 0 && is("begin");
            }
            TokenKind::Punctuation if depth == 0 && is_punct(text, t, b';') => {
                out.push(Chunk { tokens: first_token..k, span: first_byte..t.start });
                (first_token, first_byte) = (k + 1, t.end);
                (words, create, routine, begin) = (0, false, false, false);
            }
            _ => begin = false,
        }
    }
    out.push(Chunk { tokens: first_token..tokens.len(), span: first_byte..text.len() });
    out
}

/// From the first to the last significant token.
fn trimmed(tokens: &[Token]) -> Option<Range<usize>> {
    let mut significant = tokens.iter().filter(|t| t.kind.significant());
    let first = significant.next()?;
    let last = significant.next_back().unwrap_or(first);
    Some(first.start..last.end)
}

// Completion

/// Words that end a table list and so are never a table name or alias.
const NOT_NAMES: &[&str] = &[
    "ALL",
    "AND",
    "AS",
    "BY",
    "CROSS",
    "DEFAULT",
    "DELETE",
    "DO",
    "ELSE",
    "END",
    "EXCEPT",
    "EXISTS",
    "FETCH",
    "FOR",
    "FROM",
    "FULL",
    "GROUP",
    "HAVING",
    "IF",
    "IN",
    "INNER",
    "INSERT",
    "INTERSECT",
    "INTO",
    "IS",
    "JOIN",
    "LATERAL",
    "LEFT",
    "LIMIT",
    "NATURAL",
    "NOT",
    "OFFSET",
    "ON",
    "ONLY",
    "OR",
    "ORDER",
    "OUTER",
    "OVERRIDING",
    "RETURNING",
    "RIGHT",
    "SELECT",
    "SET",
    "TABLE",
    "TABLESAMPLE",
    "THEN",
    "UNION",
    "UPDATE",
    "USING",
    "VALUES",
    "WHEN",
    "WHERE",
    "WINDOW",
    "WITH",
];

fn is_punct(text: &str, t: Token, c: u8) -> bool {
    t.kind == TokenKind::Punctuation && text.as_bytes()[t.start] == c
}

/// Whether `t` is a bare word (keyword or not) that is one of `words`, compared case-insensitively.
fn is_word(text: &str, t: Token, words: &[&str]) -> bool {
    matches!(t.kind, TokenKind::Keyword | TokenKind::Identifier)
        && words.iter().any(|w| text[t.start..t.end].eq_ignore_ascii_case(w))
}

fn is_name_kind(kind: TokenKind) -> bool {
    matches!(kind, TokenKind::Keyword | TokenKind::Identifier | TokenKind::QuotedIdentifier)
}

/// Could name a table, schema or alias after `AS`.
fn is_name(text: &str, t: Token) -> bool {
    is_name_kind(t.kind) && !is_word(text, t, NOT_NAMES)
}

/// Could be an alias without `AS`: keywords are left out, so `orders where` has no alias.
fn is_bare_alias(text: &str, t: Token) -> bool {
    matches!(t.kind, TokenKind::Identifier | TokenKind::QuotedIdentifier) && !is_word(text, t, NOT_NAMES)
}

/// An identifier normalised the Postgres way: unquoted ones lowercased (ASCII only, as Postgres does),
/// quoted ones exact with `""` unescaped.
fn ident_name(text: &str, lexed: &Lexed, index: usize) -> String {
    let t = lexed.tokens[index];
    let raw = &text[t.start..t.end];
    if t.kind != TokenKind::QuotedIdentifier {
        return raw.to_ascii_lowercase();
    }
    let raw = if raw.starts_with('"') { raw } else { &raw[2..] }; // U&"..."
    let inner = &raw[1..];
    let inner = if lexed.is_open(index) { inner } else { &inner[..inner.len() - 1] };
    inner.replace("\"\"", "\"")
}

/// What fits after the significant tokens `before` (the statement up to the word being completed).
fn expect_after(text: &str, tokens: &[Token], before: &[usize]) -> Expect {
    let Some((&last, rest)) = before.split_last() else { return Expect::Any };
    let t = tokens[last];
    match t.kind {
        TokenKind::Keyword | TokenKind::Identifier => {
            if is_word(text, t, &["FROM", "JOIN", "UPDATE", "INTO", "TABLE", "ONLY", "USING", "TRUNCATE"]) {
                Expect::Table
            } else if is_word(text, t, COLUMN_AFTER)
                || is_word(text, t, &["NOT", "BETWEEN", "CASE", "WHEN", "THEN", "ELSE"])
            {
                Expect::Column
            } else {
                Expect::Any
            }
        }
        TokenKind::Operator => {
            let star = &text[t.start..t.end] == "*";
            let starts_list = rest.last().is_none_or(|&p| {
                let p = tokens[p];
                is_word(text, p, &["SELECT", "DISTINCT"]) || b",(.".iter().any(|&c| is_punct(text, p, c))
            });
            if &text[t.start..t.end] == "::" || (star && starts_list) { Expect::Any } else { Expect::Column }
        }
        TokenKind::Punctuation if is_punct(text, t, b'(') => after_paren(text, tokens, rest),
        TokenKind::Punctuation if is_punct(text, t, b',') => after_comma(text, tokens, rest),
        _ => Expect::Any,
    }
}

/// Keywords after which, and after a comma in whose clause, a column comes.
const COLUMN_AFTER: &[&str] = &["SELECT", "WHERE", "AND", "OR", "ON", "BY", "SET", "HAVING", "RETURNING", "DISTINCT"];

/// After `(`, given the tokens before it.
fn after_paren(text: &str, tokens: &[Token], before: &[usize]) -> Expect {
    match before.last() {
        Some(&p)
            if is_word(text, tokens[p], &["VALUES", "FROM", "JOIN", "LATERAL", "AS", "EXISTS", "MATERIALIZED"]) =>
        {
            Expect::Any
        }
        Some(_) => Expect::Column,
        None => Expect::Any,
    }
}

/// After `,`: whatever the enclosing clause or parenthesis takes.
fn after_comma(text: &str, tokens: &[Token], before: &[usize]) -> Expect {
    let mut depth = 0;
    for (i, &k) in before.iter().enumerate().rev() {
        let t = tokens[k];
        if is_punct(text, t, b')') {
            depth += 1;
        } else if is_punct(text, t, b'(') {
            if depth == 0 {
                return after_paren(text, tokens, &before[..i]);
            }
            depth -= 1;
        } else if depth == 0 {
            let distinct_from = i > 0 && is_word(text, tokens[before[i - 1]], &["DISTINCT"]);
            if is_word(text, t, &["FROM", "JOIN", "USING"]) && !distinct_from {
                return Expect::Table;
            }
            if is_word(text, t, COLUMN_AFTER) {
                return Expect::Column;
            }
            if is_word(text, t, &["VALUES", "WITH", "INTO", "UPDATE", "INSERT", "DELETE", "LIMIT", "OFFSET", "WINDOW"])
            {
                return Expect::Any;
            }
        }
    }
    Expect::Any
}

/// What an open parenthesis in a statement is, as far as finding tables goes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Paren {
    Plain,
    /// extract(year FROM x) and friends: that FROM is not followed by a table.
    NoTables,
    /// A subquery or function in a FROM list, which may continue after the `)`: `(select ...) s, orders`.
    FromItem,
}

/// Tables named in a statement's significant tokens, with the bytes their (schema-qualified) name covers.
fn table_refs(text: &str, lexed: &Lexed, significant: &[usize]) -> Vec<(TableRef, Range<usize>)> {
    let tokens = &lexed.tokens;
    let tok = |k: usize| significant.get(k).map(|&i| tokens[i]);
    let mut out = Vec::new();
    let mut parens: Vec<Paren> = Vec::new();
    // A table list starting at `k`; a paused one leaves `k` just inside the item's parenthesis.
    let list = |k: usize, list: bool, parens: &mut Vec<Paren>, out: &mut Vec<_>| {
        let (k, paused) = refs(text, lexed, significant, k, list, out);
        if paused {
            parens.push(Paren::FromItem);
            k + 1
        } else {
            k
        }
    };
    let mut k = 0;
    while let Some(t) = tok(k) {
        let prev = k.checked_sub(1).and_then(tok);
        if is_punct(text, t, b'(') {
            let function = ["EXTRACT", "SUBSTRING", "TRIM", "OVERLAY"];
            parens.push(if prev.is_some_and(|p| is_word(text, p, &function)) { Paren::NoTables } else { Paren::Plain });
        } else if is_punct(text, t, b')') {
            if parens.pop() == Some(Paren::FromItem) {
                // Past the item's alias, a comma continues the list.
                let mut j = k + 1;
                if tok(j).is_some_and(|t| is_word(text, t, &["AS"])) {
                    j += 1;
                }
                if tok(j).is_some_and(|t| is_bare_alias(text, t)) {
                    j += 1;
                }
                if tok(j).is_some_and(|t| is_punct(text, t, b',')) {
                    k = list(j + 1, true, &mut parens, &mut out);
                    continue;
                }
            }
        } else if is_word(text, t, &["FROM"]) {
            let not_table =
                parens.last() == Some(&Paren::NoTables) || prev.is_some_and(|p| is_word(text, p, &["DISTINCT"]));
            if !not_table {
                k = list(k + 1, true, &mut parens, &mut out);
                continue;
            }
        } else if is_word(text, t, &["JOIN", "USING", "TRUNCATE"]) {
            k = list(k + 1, true, &mut parens, &mut out);
            continue;
        } else if is_word(text, t, &["UPDATE", "INTO", "TABLE"]) {
            k = list(k + 1, false, &mut parens, &mut out);
            continue;
        } else if is_name(text, t)
            && prev.is_some_and(|p| is_word(text, p, &["WITH", "RECURSIVE"]) || is_punct(text, p, b','))
            && is_cte(text, tokens, &significant[k + 1..])
        {
            let name = ident_name(text, lexed, significant[k]);
            out.push((TableRef { schema: None, name, alias: None }, t.start..t.end));
        }
        k += 1;
    }
    out
}

/// Whether the tokens after a name continue a CTE: `[(columns)] AS [NOT] [MATERIALIZED] (`.
fn is_cte(text: &str, tokens: &[Token], after: &[usize]) -> bool {
    let tok = |j: usize| after.get(j).map(|&i| tokens[i]);
    let mut j = 0;
    if tok(j).is_some_and(|t| is_punct(text, t, b'(')) {
        j += 1;
        while tok(j).is_some_and(|t| is_name_kind(t.kind) || is_punct(text, t, b',')) {
            j += 1;
        }
        if !tok(j).is_some_and(|t| is_punct(text, t, b')')) {
            return false;
        }
        j += 1;
    }
    if !tok(j).is_some_and(|t| is_word(text, t, &["AS"])) {
        return false;
    }
    j += 1;
    while tok(j).is_some_and(|t| is_word(text, t, &["NOT", "MATERIALIZED"])) {
        j += 1;
    }
    tok(j).is_some_and(|t| is_punct(text, t, b'('))
}

/// Reads `[schema.]name [[AS] alias]` from significant token `k` on, repeated over commas when `list`.
/// Returns the index of the first token it did not take, and whether the list paused at the `(` of a
/// subquery or function (`from (select ...) s`, `from generate_series(1, 3) g`), which are not tables.
fn refs(
    text: &str,
    lexed: &Lexed,
    significant: &[usize],
    mut k: usize,
    list: bool,
    out: &mut Vec<(TableRef, Range<usize>)>,
) -> (usize, bool) {
    let tok = |k: usize| significant.get(k).map(|&i| lexed.tokens[i]);
    let name = |k: usize| ident_name(text, lexed, significant[k]);
    loop {
        while tok(k).is_some_and(|t| is_word(text, t, &["ONLY", "LATERAL", "IF", "NOT", "EXISTS"])) {
            k += 1;
        }
        if list && tok(k).is_some_and(|t| is_punct(text, t, b'(')) {
            return (k, true);
        }
        let Some(first) = tok(k).filter(|&t| is_name(text, t)) else { return (k, false) };
        let mut parts = vec![name(k)];
        let mut end = first.end;
        k += 1;
        while let Some(dot) = tok(k).filter(|&t| is_punct(text, t, b'.')) {
            end = dot.end;
            k += 1;
            let Some(part) = tok(k).filter(|&t| is_name_kind(t.kind)) else { break };
            parts.push(name(k));
            end = part.end;
            k += 1;
        }
        // In a FROM list `name(` is a function call; after INTO it opens a column list.
        if list && tok(k).is_some_and(|t| is_punct(text, t, b'(')) {
            return (k, true);
        }
        let alias = match tok(k) {
            Some(t) if is_word(text, t, &["AS"]) => {
                k += 1;
                tok(k).filter(|&t| is_name(text, t)).map(|_| {
                    k += 1;
                    name(k - 1)
                })
            }
            Some(t) if is_bare_alias(text, t) => {
                k += 1;
                Some(name(k - 1))
            }
            _ => None,
        };
        let Some(table) = parts.pop() else { return (k, false) };
        out.push((TableRef { schema: parts.pop(), name: table, alias }, first.start..end));
        if !(list && tok(k).is_some_and(|t| is_punct(text, t, b','))) {
            return (k, false);
        }
        k += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use TokenKind::*;

    fn kinds(text: &str) -> Vec<(TokenKind, &str)> {
        tokenize(text).into_iter().filter(|t| t.kind != Whitespace).map(|t| (t.kind, &text[t.start..t.end])).collect()
    }

    fn texts(text: &str) -> Vec<&str> {
        statements(text).into_iter().map(|r| &text[r]).collect()
    }

    /// Completion at the `|` in `text`.
    fn complete(text: &str) -> Option<Completion> {
        let cursor = text.find('|').expect("cursor marker");
        completion_context(&text.replace('|', ""), cursor)
    }

    fn table(schema: Option<&str>, name: &str, alias: Option<&str>) -> TableRef {
        TableRef { schema: schema.map(Into::into), name: name.into(), alias: alias.map(Into::into) }
    }

    #[test]
    fn keywords_are_sorted_uppercase_and_cover_the_list_enders() {
        assert!(KEYWORDS.windows(2).all(|w| w[0] < w[1]));
        assert!(KEYWORDS.iter().all(|k| k.bytes().all(|b| b.is_ascii_uppercase() || b == b'_')));
        assert!(NOT_NAMES.iter().all(|w| is_keyword(w)));
        assert!(is_keyword("select") && is_keyword("Coalesce") && is_keyword("NOW"));
        assert!(!is_keyword("orders") && !is_keyword("") && !is_keyword("selectx") && !is_keyword("å"));
    }

    #[test]
    fn tokenizes_each_kind() {
        assert_eq!(
            kinds("SELECT o.id, \"Mixed \"\"Q\"\"\" FROM orders o WHERE total >= 1.5e3 AND x = $1;"),
            [
                (Keyword, "SELECT"),
                (Identifier, "o"),
                (Punctuation, "."),
                (Identifier, "id"),
                (Punctuation, ","),
                (QuotedIdentifier, "\"Mixed \"\"Q\"\"\""),
                (Keyword, "FROM"),
                (Identifier, "orders"),
                (Identifier, "o"),
                (Keyword, "WHERE"),
                (Identifier, "total"),
                (Operator, ">="),
                (Number, "1.5e3"),
                (Keyword, "AND"),
                (Identifier, "x"),
                (Operator, "="),
                (Parameter, "$1"),
                (Punctuation, ";"),
            ]
        );
        assert_eq!(
            kinds("'it''s' E'a\\'b' $$ x ' $$ $fn$ $$ $fn$ -- note\n/* a /* nested */ c */ x::int"),
            [
                (String, "'it''s'"),
                (String, "E'a\\'b'"),
                (String, "$$ x ' $$"),
                (String, "$fn$ $$ $fn$"),
                (Comment, "-- note"),
                (Comment, "/* a /* nested */ c */"),
                (Identifier, "x"),
                (Operator, "::"),
                (Keyword, "int"),
            ]
        );
        assert_eq!(
            kinds("data->>'k' @> b <> c || 0x1F a.date"),
            [
                (Identifier, "data"),
                (Operator, "->>"),
                (String, "'k'"),
                (Operator, "@>"),
                (Identifier, "b"),
                (Operator, "<>"),
                (Identifier, "c"),
                (Operator, "||"),
                (Number, "0x1F"),
                (Identifier, "a"),
                (Punctuation, "."),
                (Identifier, "date"),
            ]
        );
        assert_eq!(kinds("a+--b"), [(Identifier, "a"), (Operator, "+"), (Comment, "--b")]);
        assert_eq!(kinds("'open"), [(String, "'open")]);
        assert_eq!(kinds("/* open"), [(Comment, "/* open")]);
        assert_eq!(kinds("\"open"), [(QuotedIdentifier, "\"open")]);
        assert_eq!(kinds("$x$ open"), [(String, "$x$ open")]);
        assert_eq!(kinds("namn = 'Åsa 山 🦊'"), [(Identifier, "namn"), (Operator, "="), (String, "'Åsa 山 🦊'")]);
    }

    #[test]
    fn splits_on_semicolons_outside_quotes_comments_and_bodies() {
        assert_eq!(texts("select 1; select 2"), ["select 1", "select 2"]);
        assert_eq!(texts("select ';' ; select \"a;b\";"), ["select ';'", "select \"a;b\""]);
        assert_eq!(texts("select 1 -- not; here\n; /* or; here */ select 2"), ["select 1", "select 2"]);
        assert_eq!(
            texts("create function f() returns int as $$ begin; return 1; end $$ language plpgsql; select f()"),
            ["create function f() returns int as $$ begin; return 1; end $$ language plpgsql", "select f()"]
        );
        assert_eq!(
            texts(
                "create function g() returns int begin atomic select 1; select case when true then 2 end; end; select 3"
            ),
            [
                "create function g() returns int begin atomic select 1; select case when true then 2 end; end",
                "select 3"
            ]
        );
        assert_eq!(texts("begin; select 1; commit;"), ["begin", "select 1", "commit"]);
        assert_eq!(texts(" ;; -- only a comment\n ; \n"), Vec::<&str>::new());
        assert_eq!(texts("select 'unterminated; select 2"), ["select 'unterminated; select 2"]);
    }

    #[test]
    fn finds_the_statement_at_the_cursor() {
        let text = "select 1;\n\nselect 2;  \n  select 3";
        let at = |cursor: usize| statement_at(text, cursor).map(|r| &text[r]);
        assert_eq!(at(0), Some("select 1"));
        assert_eq!(at(4), Some("select 1"));
        assert_eq!(at(8), Some("select 1")); // right before the ;
        assert_eq!(at(9), Some("select 1")); // right after it, same line
        assert_eq!(at(10), Some("select 1")); // blank line between
        assert_eq!(at(text.find("2;").unwrap() + 4), Some("select 2")); // trailing spaces on its line
        assert_eq!(at(text.rfind('\n').unwrap() + 1), Some("select 3")); // indentation before it
        assert_eq!(at(text.len()), Some("select 3"));
        assert_eq!(at(text.len() + 10), Some("select 3"));
        assert_eq!(statement_at("\n\n  select 1", 0).map(|r| r.start), Some(4)); // nothing before: the next one
        assert_eq!(statement_at("  -- just a comment ", 3), None);
        assert_eq!(statement_at("", 0), None);
    }

    #[test]
    fn expects_tables_after_from_join_and_from_lists() {
        let c = complete("select * from |").unwrap();
        assert_eq!((c.prefix.as_str(), c.start, c.expect, c.qualifier), ("", 14, Expect::Table, None));
        assert!(c.tables.is_empty());

        let c = complete("select * from orders o left join cust|").unwrap();
        assert_eq!((c.prefix.as_str(), c.start, c.expect), ("cust", 33, Expect::Table));
        assert_eq!(c.tables, [table(None, "orders", Some("o"))]);

        let c = complete("select * from orders o, |").unwrap();
        assert_eq!(c.expect, Expect::Table);
        assert_eq!(c.tables, [table(None, "orders", Some("o"))]);

        for text in ["update |", "insert into |", "delete from |", "truncate table |"] {
            assert_eq!(complete(text).unwrap().expect, Expect::Table, "{text}");
        }
    }

    #[test]
    fn schema_qualifier_expects_a_table() {
        let c = complete("select * from public.ord|").unwrap();
        assert_eq!((c.prefix.as_str(), c.qualifier.as_deref(), c.expect), ("ord", Some("public"), Expect::Table));
        assert!(c.tables.is_empty());
        let c = complete("select * from Public.|").unwrap();
        assert_eq!((c.prefix.as_str(), c.qualifier.as_deref(), c.expect), ("", Some("public"), Expect::Table));
    }

    #[test]
    fn alias_qualifier_expects_its_columns() {
        let c = complete("select o.| from orders as o join public.customers c on c.id = o.customer_id").unwrap();
        assert_eq!((c.prefix.as_str(), c.qualifier.as_deref(), c.expect), ("", Some("o"), Expect::Column));
        assert_eq!(c.tables, [table(None, "orders", Some("o")), table(Some("public"), "customers", Some("c"))]);

        let c = complete("select * from \"Mixed Case\" m where m.na|").unwrap();
        assert_eq!((c.prefix.as_str(), c.qualifier.as_deref(), c.expect), ("na", Some("m"), Expect::Column));
        assert_eq!(c.tables, [table(None, "Mixed Case", Some("m"))]);

        let c = complete("select \"Mixed Case\".| from \"Mixed Case\"").unwrap();
        assert_eq!((c.qualifier.as_deref(), c.expect), (Some("Mixed Case"), Expect::Column));
    }

    #[test]
    fn expects_columns_in_clauses_with_the_tables_known() {
        let c = complete("select id, | from orders o join customers on true where o.total > 1").unwrap();
        assert_eq!(c.expect, Expect::Column);
        assert_eq!(c.tables, [table(None, "orders", Some("o")), table(None, "customers", None)]);

        for text in [
            "select |",
            "select * from orders where |",
            "select * from orders where total > 1 and |",
            "select * from a join b on |",
            "select * from orders order by total, |",
            "select count(|",
            "update orders set |",
            "update orders set a = 1, |",
            "insert into orders (|",
            "delete from orders where id = |",
            "select * from orders returning |",
        ] {
            let c = complete(text).unwrap();
            assert_eq!(c.expect, Expect::Column, "{text}");
            assert!(!c.tables.is_empty() || text == "select |" || text == "select count(|", "{text}");
        }
        for text in ["|", "select * |", "select * from orders |", "select x::|", "insert into t values (|"] {
            assert_eq!(complete(text).unwrap().expect, Expect::Any, "{text}");
        }
    }

    #[test]
    fn completes_the_word_up_to_the_cursor() {
        let c = complete("select * from orders whe|re").unwrap();
        assert_eq!((c.prefix.as_str(), c.start, c.expect), ("whe", 21, Expect::Any));
        let c = complete("sel|").unwrap();
        assert_eq!((c.prefix.as_str(), c.start), ("sel", 0));
    }

    #[test]
    fn uses_only_the_statement_at_the_cursor() {
        let c = complete("select * from a; select | from b; select * from c").unwrap();
        assert_eq!(c.tables, [table(None, "b", None)]);
        let c = complete("select * from a;|").unwrap();
        assert_eq!((c.expect, c.tables.len()), (Expect::Any, 0));
    }

    #[test]
    fn collects_ctes_and_ignores_froms_that_are_not_tables() {
        let c = complete(
            "with recent (id) as (select id from orders), top as materialized (select 1) select | from recent",
        )
        .unwrap();
        assert_eq!(c.tables, [table(None, "recent", None), table(None, "orders", None), table(None, "top", None)]);
        let c = complete("select extract(year from created), a is distinct from b, | from t").unwrap();
        assert_eq!(c.tables, [table(None, "t", None)]);
        let c = complete("select | from generate_series(1, 3) g, orders").unwrap();
        assert_eq!(c.tables, [table(None, "orders", None)]);
        let c = complete("select | from (select * from a) s, b join lateral (select 1) l on true join c using (id)")
            .unwrap();
        assert_eq!(c.tables, [table(None, "a", None), table(None, "b", None), table(None, "c", None)]);
    }

    #[test]
    fn offers_nothing_inside_strings_comments_quotes_or_numbers() {
        for text in [
            "select * from t where a = 'ab|c'",
            "select * from t where a = 'ab|",
            "select 1 -- comm|",
            "select 1 -- comment|\n",
            "select /* a | */ 1",
            "select \"Mix|",
            "select 12|",
            "select $1|",
            "select $$ bo|dy $$",
        ] {
            assert_eq!(complete(text), None, "{text}");
        }
        assert!(complete("select 'abc'|").is_some());
        assert!(complete("select /* done */|").is_some());
        assert!(complete("select 1 -- comment\n|").is_some());
    }

    #[test]
    fn survives_every_cursor_in_nasty_text() {
        let nasty = [
            "select 'it''s E'\\' from \"Odd \"\" Name\" o where o.a->>'k' @> '{}' and b <> $1::int; -- å 山 🦊",
            "with x as (select $$ ; 🦊 $$, $tag$ $$ ; $tag$ from public.\"t🦊\" t) select t.| from x;; /* open /* nested",
            "select E'unterminated \\' 😀 ; select \"open ident ; 山",
            "insert into t (a, b) values (1.5e3, 0x1F), (.5, 'x') returning *; update t set a = a::text || 'å';",
            "create function f() returns int begin atomic select 1; end; $ $1 $a $ab$ 1..2 :: := ->> @> <> ~* !~",
        ];
        for text in nasty {
            for (end, _) in text.char_indices().chain([(text.len(), ' ')]) {
                let prefix = &text[..end];
                let tokens = tokenize(prefix);
                let mut offset = 0;
                for t in &tokens {
                    assert!(t.start == offset && t.end > t.start, "{prefix:?}");
                    assert!(prefix.is_char_boundary(t.end), "{prefix:?}");
                    offset = t.end;
                }
                assert_eq!(offset, prefix.len());
                for range in statements(prefix) {
                    assert!(!prefix[range].trim().is_empty());
                }
                for cursor in 0..=prefix.len() + 1 {
                    if let Some(range) = statement_at(prefix, cursor) {
                        let _ = &prefix[range];
                    }
                    if let Some(c) = completion_context(prefix, cursor) {
                        assert!(prefix.is_char_boundary(c.start) && c.start <= cursor.min(prefix.len()));
                        assert_eq!(&prefix[c.start..c.start + c.prefix.len()], c.prefix);
                    }
                }
            }
        }
    }
}
