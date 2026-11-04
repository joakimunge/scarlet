mod agent;
mod app;
mod complete;
mod connect;
mod db;
mod scratch;
mod sql;
mod ui;
mod vimkeys;

use std::io;

use std::{path::PathBuf, sync::Arc};

use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand};
use crossterm::{
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute,
};

/// A fast, keyboard-driven terminal client for PostgreSQL.
#[derive(Parser)]
#[command(version, about, args_conflicts_with_subcommands = true)]
struct Cli {
    /// Connection URL or key=value string, e.g. postgres://user@localhost/shop.
    /// Without it, Scarlet uses .env files, PG* variables and local discovery.
    connection: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Serve a database to AI agents over MCP (stdio), read-only.
    Mcp {
        /// Connection URL, key=value string or database name; defaults like Scarlet's own.
        connection: Option<String>,
        /// Used by Scarlet itself: pass tool calls to a running Scarlet.
        #[arg(long, hide = true, conflicts_with = "connection")]
        bridge: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(Command::Mcp { connection, bridge }) = cli.command {
        return mcp(connection, bridge).await;
    }
    let initial = cli.connection.as_deref().map(parse_target);

    // ratatui::init chains its own hook that restores the terminal before this one runs.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(io::stdout(), DisableBracketedPaste);
        previous(info);
    }));
    let mut terminal = ratatui::init();
    execute!(io::stdout(), EnableBracketedPaste)?;
    let result = app::run(&mut terminal, initial).await;
    let _ = execute!(io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    result
}

fn parse_target(input: &str) -> connect::Target {
    match connect::Target::parse(input, connect::Source::Argument) {
        Ok(target) => target,
        Err(e) => {
            eprintln!("scarlet: {e}");
            std::process::exit(2);
        }
    }
}

async fn mcp(connection: Option<String>, bridge: Option<PathBuf>) -> Result<()> {
    if let Some(path) = bridge {
        return agent::mcp::serve(agent::mcp::Backend::Bridge(path)).await;
    }
    let target = match connection {
        Some(input) => parse_target(&input),
        None => connect::discover::env_target()
            .or_else(|| connect::discover::dotenv_targets(&std::env::current_dir().ok()?).into_iter().next())
            .ok_or_else(|| anyhow!("no database given: pass a connection, or set DATABASE_URL or PG* variables"))?,
    };
    let session = db::postgres::Session::connect(&target, |_| std::process::exit(1))
        .await
        .map_err(|e| anyhow!("{}: {}", target.database(), e.message))?;
    agent::mcp::serve(agent::mcp::Backend::Direct(Arc::new(session))).await
}
