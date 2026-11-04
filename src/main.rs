mod app;
mod complete;
mod connect;
mod db;
mod scratch;
mod sql;
mod ui;
mod vimkeys;

use std::io;

use anyhow::Result;
use clap::Parser;
use crossterm::{
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute,
};

/// A fast, keyboard-driven terminal client for PostgreSQL.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Connection URL or key=value string, e.g. postgres://user@localhost/shop.
    /// Without it, Scarlet uses .env files, PG* variables and local discovery.
    connection: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let initial = match cli.connection.as_deref() {
        Some(input) => match connect::Target::parse(input, connect::Source::Argument) {
            Ok(target) => Some(target),
            Err(e) => {
                eprintln!("scarlet: {e}");
                std::process::exit(2);
            }
        },
        None => None,
    };

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
