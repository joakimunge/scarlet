# Scarlet

A fast, keyboard-only terminal client for PostgreSQL. Other databases will follow.

## Install

```sh
brew install joakimunge/tap/scarlet
```

Or without Homebrew, on macOS or Linux (installs to `~/.local/bin`):

```sh
curl -fsSL https://raw.githubusercontent.com/joakimunge/scarlet/main/install.sh | sh
```

Or from source, with Rust 1.88 or newer:

```sh
cargo install --git https://github.com/joakimunge/scarlet --locked
```

That's all it needs. For [Ask](#ask), which answers questions in plain language, also install
[Claude Code](https://claude.com/code) and sign in once with `claude`. Ask uses your own Claude
account; Scarlet never sees or stores your credentials.

### Updating

Scarlet checks for a new release each time it starts (one small request to GitHub, skipped when
`SCARLET_NO_UPDATE_CHECK` is set). When there is one, a note in the corner says so, with the command
for the way you installed it. `ctrl+k` → *Update Scarlet* runs that command for you and starts the new
version where you left off.

## Use

```sh
scarlet                                  # .env, PG* variables or local discovery
scarlet shop                             # a database on the local server
scarlet postgres://user@host:5432/shop   # any URL or key=value connection string
```

## Connecting

Scarlet looks for databases without any setup:

1. A connection given on the command line.
2. Postgres URLs in `.env`, `.env.local`, `.env.development` and `.env.development.local`,
   from the current directory up to the project root.
3. `DATABASE_URL`, or `PGHOST` / `PGPORT` / `PGDATABASE` / `PGUSER`.
4. Running Docker containers that publish port 5432. Credentials come from the container's `POSTGRES_*` environment.
5. Servers on this machine (Unix sockets and common ports), with every database they hold.

If exactly one connection comes from the command line or `.env`, Scarlet connects straight away.
Otherwise, it opens a picker where you can search, or paste a URL. Passwords come from the URL,
`PGPASSWORD` or `~/.pgpass`. If one is still needed, Scarlet asks for it.

Unknown URL parameters (Prisma's `schema`, `connection_limit`, ...) are ignored. As in libpq,
`sslmode=require` encrypts without verifying the certificate, and `verify-ca` / `verify-full` verify it.

## Keys

The footer always shows the keys that work right now. `space` (or your vim leader) opens a menu of
everything you can do where you are, and `?` lists every key.

| Keys | |
|---|---|
| `↑ ↓ ← →` or `h j k l` | move |
| `page up/down`, `ctrl+u/d` | scroll a page / half a page |
| `home end`, `g g`, `G` | first / last row |
| `0` `$` | first / last column |
| `tab` | switch between tables and data |
| `enter` | open a table; on a foreign key, go to the row it points at; on a key others point at, show those rows; otherwise view the value |
| `esc`, `backspace`, `ctrl+o` | back along followed links, then clear the filter, then to the tables |
| `/` | filter: words search whole rows as you type, SQL (`total > 100`) becomes a condition |
| `=` `!` | only rows with / without the value under the cursor |
| `s` | sort by this column: ascending, descending, off |
| `v` | the whole row, vertically |
| `c` `y` / `Y` | copy the value / the row as JSON |
| `r` | reload |
| `a` | ask about the data in plain words |
| `w` | what your last question sent to Claude |
| `ctrl+k` | jump to any table or run a command |
| `q`, `ctrl+c` | quit |

### SQL

`ctrl+e` opens the editor above a result pane. If the editor is empty, it starts with the table you
were browsing, already written as SQL with its filter and sort.

- `ctrl+r` runs the statement under the cursor. The bar in the gutter shows which one that is.
- Completion knows your schema: tables after `FROM` / `JOIN`, the query's columns elsewhere, and
  `alias.` gives that table's columns. Keywords come in the case you type them. `ctrl+space` asks for
  suggestions, and `tab` or `enter` accepts one.
- The editor is read-only. A statement that changes data stops and asks first, and `ctrl+r` again runs
  it once in its own transaction.
- Results are kept up to 50,000 rows. `/` searches them and `s` sorts them, all locally.
  `esc` stops a running query.
- `ctrl+o` opens the text in `$SCARLET_EDITOR`, `$VISUAL` or `$EDITOR`, and takes it back when you quit.
- The editor's text is kept per database between sessions.

### Ask

`a` asks about the data in plain words: "which countries have the most VIP customers". Claude reads the
schema, tries a few small queries to see what the values look like, then shows its answer as rows, with
the SQL added to the editor under your question so you can change it and run it again.

- The next question follows on from the last one ("only the Nordic ones"). `ctrl+n` in the question
  box starts over.
- Claude can only read. Its queries run one statement at a time in a transaction that is always rolled
  back, on a connection of its own.
- It runs Claude Code headless, signed in as you, with none of your own settings, plugins or tools.
  Set `SCARLET_MODEL` to pick another model (default: `sonnet`). `esc` stops it.

#### Where your data goes

Ask sends your question to Anthropic through your own Claude account, together with what Claude reads
from the database. The first time you ask on a database, Scarlet asks how much Claude may see, and
remembers it (`ctrl+p` in the question box changes it):

- **Masked values** (recommended): Claude can look at values to get the details right, but emails,
  names, phone numbers, addresses, ID and card numbers, IP addresses and secrets are replaced with
  stand-ins like `‹email 3›` before they leave your computer. Columns are judged by name, type and
  where a result column comes from, so renaming one in a query doesn't reveal it, and other text is
  searched for emails, personal identity numbers, phone and card numbers. JSON is masked key by key.
- **Table layout only**: table and column names, no values at all. Answers are a little less accurate.
- **All values**: for your own test data.

Whatever you choose, the rows of an answer never go to Claude; they only appear on your screen. `w`
shows exactly what your last question sent. Masking is careful but not a guarantee: for real
customer data, give Ask a database role that can only read what it should.

Your Claude account's own settings decide what Anthropic keeps: on Pro and Max plans, check the
privacy setting for model training; Team, Enterprise and API accounts are not used for training.

`scarlet mcp` serves the same read-only tools to any MCP client, masked unless you pass
`--values full` (or `--values schema`). To use a database from Claude Code:

```sh
claude mcp add scarlet -- scarlet mcp postgres://localhost/shop
```

### Your vim keys

If you use Neovim (LazyVim and friends included) or Vim, Scarlet asks your editor for its normal-mode
mappings and adopts the ones that move: remapped `j`/`k`, `<C-d>zz`, a `€` for `$` on a Swedish keyboard,
`<C-h>`/`<C-l>` window or tmux navigation to switch panes. The result is cached until your config changes.
If lazy.nvim has plugins it would need to install, Scarlet skips this rather than touch your setup.

## Look

Scarlet draws with your terminal's own colours: its foreground, background and ANSI palette, so it
matches whatever theme you use. The one exception is the wordmark on the start screen, which is
always scarlet (a smooth sweep where `COLORTERM` says 24-bit colour, whole-letter steps otherwise).
`NO_COLOR` is respected.

## Development

```sh
cargo run -- scarlet_dev
cargo test
cargo clippy --all-targets
```

`dev/seed.sql` builds a fixture database full of edge cases: JSONB, NULLs, emoji, long and multi-line
text, 200k rows, a wide table, views, a materialized view, a second schema and quoted identifiers.

```sh
createdb scarlet_dev && psql -d scarlet_dev -f dev/seed.sql
```

### How it stays fast

- The UI loop only draws from state. All database work runs on background tasks and reports back
  as messages, so a slow query never freezes the screen.
- Rows load in pages of 500 around the viewport. Tables of any size open instantly, and `G` on
  200k rows fetches only the last page.
- Tables up to 256 MB get an exact row count. Bigger ones show the planner's estimate (`~1.2M`),
  so nothing expensive runs behind your back. Counts run on a second connection with a 5-second
  statement timeout.
- Paging orders by the primary key, so pages never overlap.
- A query that runs longer than 250 ms is cancelled when you move on, so one slow sort or view never
  holds up the next table.
- Data queries run read-only, and a filter can't contain `;`. Nothing typed into Scarlet can change data.
