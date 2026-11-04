# CLAUDE.md

Guidance for Claude Code (and any other agent) working in this repository.

## Project

Scarlet is a keyboard-only terminal client for PostgreSQL, written in Rust (other databases later).
It is a personal tool, macOS first. In order of priority:

1. **Usability.** It should be unimaginably intuitive. The context key footer, the `ctrl+k`
   palette, the `space` menu and `?` are the main bet: nobody should need to remember a key.
2. **Speed.** The UI never waits on the database.
3. **Polish.** The bar is "gorgeous, no mistakes".

The README is the user-facing manual. Keep it in step with every feature and key change.

## Commands

```sh
cargo run -- scarlet_dev          # run against the fixture database
cargo test                        # unit tests
cargo test -- --ignored           # also the tests that need scarlet_dev, nvim or vim
cargo clippy --all-targets        # must stay at zero warnings
cargo fmt                         # rustfmt.toml: max_width 120
cargo install --path .            # the user's installed `scarlet`
```

Fixture database, full of edge cases (JSONB, NULLs, emoji, 200k rows, quoted names, views, a second schema):

```sh
createdb scarlet_dev && psql -d scarlet_dev -f dev/seed.sql
```

The toolchain is pinned in `rust-toolchain.toml` (stable, edition 2024). Leave the user's global
rustup default alone.

## Architecture

- `src/app.rs`: state, keys and the event loop. The loop only draws from state. Database work runs
  on spawned tasks that report back as `Msg`. Generation ids drop stale results.
- `src/ui/`: one component per file, each owning its view state and rendering itself. `ui/mod.rs`
  lays them out. `theme.rs` holds every style.
- `src/db/postgres.rs`: one `Session` with separate connections:
  - main: browsing, read-only
  - `aux`: counts, read-only, 5 s timeout
  - lazy `editor` connection: read-only unless a write is confirmed
  - lazy `agent` connection: every statement runs in a rolled-back read-only transaction
- `src/connect/`: zero-setup connections:
  - CLI argument or bare database name
  - `.env` files up to the git root
  - `DATABASE_URL` / `PG*` variables and pgpass
  - Docker containers
  - local sockets and ports
- `src/sql.rs` and `src/complete.rs`: tokenizer, statement detection and schema-aware completion.
- `src/vimkeys.rs`: reads the user's own vim/nvim keymaps (headless nvim, cached). It must never write
  to their config. It skips loading when lazy.nvim plugins are missing.
- `src/agent/`: Ask, plain-language questions answered by Claude Code headless.
  - Claude gets only Scarlet's tools: `--setting-sources ""`, `--strict-mcp-config`, `--tools ""`.
  - Tool calls come back through `scarlet mcp --bridge` to the running session.
  - `scarlet mcp <connection>` serves the same read-only tools standalone.
  - `agent/mask.rs`: what Claude may see per database (masked by default, asked on first use) and the
    masking itself. Answer rows never go back to Claude. Anything that can carry values to the agent
    must pass through the `Masker`.

## Product rules

- **Colours come from the terminal theme**: default foreground/background and the ANSI palette, with
  ANSI blue as the accent. The only exception is the scarlet wordmark on the start screen.
  Respect `NO_COLOR`.
- **Hybrid keys.** Arrows, `enter`, `esc` and the palette work for everyone. `hjkl`, `gg`, `G` and the
  user's own vim mappings work too. Modes exist only inside the SQL editor.
- **Read-only by default.** Browsing and filters can never change data. A write from the editor needs
  a second `ctrl+r` and runs in its own transaction. The agent can never write.
- **Stay calm and quiet.** Every label earns its place, each piece of information is shown once, and
  each colour has one job. Use plain sentence-case labels, no jokey branding, no shouty mono headers.
- **Show what keys do, where they do it.** A new key goes in the footer hints, the `space` menu and
  the `?` help when it applies there, and in the README.
- **Planned (not built yet):**
  - M4, safe editing: staged cell edits reviewed as an SQL diff; dev/staging/prod connection tags
    with prod read-only.
  - Later: saved connections in the keychain, and other databases.

## Working rules

- **Commit only when the user says "commit".** Ask before pushing, opening PRs or creating tickets.
  The GitHub repo is `joakimunge/scarlet` (public), so nothing personal goes into files or commits:
  commits use the GitHub noreply address, and examples use `example.com`.
- **Never read or print secrets.** `.env` values and passwords are referred to by path or name only.
- **Ask before non-obvious design choices.** Once the direction is clear, make small calls yourself
  and list them for veto.
- **Pre-existing problems found mid-task:** say they aren't from this change and suggest a follow-up.
  Don't widen the scope.
- **Notes about the work go in this repository** (this file, `NOTES.md`), not in agent memory.
- **Keep the code professional.** No persona, chat tone or helper names in code, comments, commits or
  docs. Write code that reads like the code around it.

## Testing

- **Write only the few tests that pin down the rule or bug at hand**, asserting observable behaviour.
  No tests of query shape, wiring or defaults. Database-backed tests are `#[ignore]` and use
  `scarlet_dev`.
- **Verify features in a real terminal against real data** before calling them done. Drive Scarlet in
  a detached tmux session:
  - Always target it exactly: `tmux send-keys -t '=sc-test:' …`. Check
    `tmux has-session -t '=sc-test'` first.
  - A prefix match once typed keys into the user's own session. Their sessions are named after
    projects.
  - Don't use foreground `sleep`; use `perl -e 'select(undef,undef,undef,0.6)'`.
  - Mind where focus is: in the SQL editor every letter (including `q` and `a`) is typed text.
    `ctrl+c` always quits.
  - Afterwards kill the test session, check that no `scarlet mcp` or `claude -p` processes are left,
    and restore any scratch SQL the test changed. The scratch lives in
    `~/Library/Application Support/scarlet/scratch/`.
- **Visual changes get real screenshots for approval, never mockups.** Capture the pane with
  `tmux capture-pane -p -e -t '=sc-test:' > shot.txt`, then render it with
  `python3 -I dev/ansi2png.py shot.txt shot.png 120` (add `light` for a light background). Review
  light and dark, and 256-colour (`env -u COLORTERM`) as well as 24-bit.
- **Before handing over**, run `cargo fmt`, clippy with zero warnings, and `cargo test`. Report
  plainly what was checked and what couldn't be.

## Releasing

CI (`.github/workflows/ci.yml`) runs fmt, clippy, the tests on macOS and Linux, the database test
against a Postgres service, and a build with the minimum Rust version.

To release (only when the user asks):

1. Bump `version` in `Cargo.toml`, run `cargo check` so `Cargo.lock` follows, and commit.
2. Tag and push the tag:

   ```sh
   git tag v0.2.0 && git push origin v0.2.0
   ```

`.github/workflows/release.yml` then does the rest:
- builds macOS (Apple silicon and Intel) and Linux (x86_64 and arm64) binaries
- publishes them with checksums as a GitHub release
- writes the formula with `dev/formula.sh` and pushes it to `joakimunge/homebrew-tap`, using the
  `TAP_DEPLOY_KEY` secret (a deploy key that can write to that one repo)

`install.sh` always installs the latest release. Scarlet itself notices new releases at start
(`src/update.rs`) and can update the way it was installed (Homebrew, cargo or the script), then
relaunches. Updates never touch shell startup files.
