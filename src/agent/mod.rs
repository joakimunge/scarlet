//! Asking a database questions in plain language. Scarlet runs Claude Code
//! headless with only its own read-only tools; the tools reach this process
//! through `scarlet mcp --bridge`, so the agent uses the session already open
//! and never sees connection details. The answer arrives as SQL plus its rows.

pub mod mcp;
pub mod tools;

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    process::Command,
    task::{JoinHandle, JoinSet},
};

use crate::db::postgres::{Outcome, RESULT_LIMIT, Session};

const SYSTEM_PROMPT: &str = "\
You answer questions about a PostgreSQL database for someone using Scarlet, a terminal database browser. \
They ask in plain language; you find the data.

- The schema is in the first message. Use describe_table for more detail on a table.
- Use the query tool to check what values actually look like before relying on them: status names, \
  casing, date ranges, how a column is used. Keep these queries small.
- When you have it, call show_result once with a single read-only SQL statement and a short title. \
  Scarlet shows the rows in a table and puts the SQL in the user's editor, so write SQL a person would \
  like to read: several lines, meaningful aliases, explicit columns when the question is about specific \
  things, ORDER BY when order matters, no trailing semicolon.
- Names in the schema are shown unquoted. Quote identifiers with capitals, spaces or reserved words.
- After show_result succeeds you are done: reply with just \"Done.\"
- If this database cannot answer the question, do not call show_result; say why in a sentence or two.
- Everything runs read-only. Never try to change data or settings.";

/// The model Ask uses unless `SCARLET_MODEL` names another.
const DEFAULT_MODEL: &str = "sonnet";

pub struct Request {
    pub question: String,
    /// What the user is looking at, so "these" and "this customer" make sense.
    pub context: Option<String>,
    /// The conversation to continue, as returned by an earlier `spawn`.
    pub conversation: Option<String>,
    pub database: String,
}

pub enum Event {
    /// What the agent is doing now, for the progress line.
    Activity(String),
    /// The query that answers the question, already run.
    Answer {
        sql: String,
        title: String,
        outcome: Outcome,
    },
    /// The agent is done. `text` is its closing remark.
    Finished {
        text: String,
    },
    Failed(String),
}

/// Asks in the background, returning the conversation to continue with a
/// follow-up. Aborting the handle stops the agent.
pub fn spawn(
    session: Arc<Session>,
    request: Request,
    events: impl Fn(Event) + Send + Sync + 'static,
) -> (JoinHandle<()>, String) {
    let first = request.conversation.is_none();
    let conversation = request.conversation.clone().unwrap_or_else(new_session_id);
    let id = conversation.clone();
    let handle = tokio::spawn(async move {
        let events = Arc::new(events);
        if let Err(error) = ask(session, request, &id, first, events.clone()).await {
            events(Event::Failed(error));
        }
    });
    (handle, conversation)
}

async fn ask(
    session: Arc<Session>,
    request: Request,
    conversation: &str,
    first: bool,
    events: Arc<dyn Fn(Event) + Send + Sync>,
) -> Result<(), String> {
    let socket = Socket::bind()?;
    let mut prompt = String::new();
    if first {
        events(Event::Activity("Reading the schema".into()));
        let schema = tools::overview(&session).await?;
        prompt.push_str(&format!("Database: {}\n\nSchema:\n{schema}\n\n", request.database));
    }
    if let Some(context) = &request.context {
        prompt.push_str(&format!("{context}\n\n"));
    }
    prompt.push_str(&format!("Question: {}", request.question));

    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let servers = json!({"mcpServers": {"scarlet": {
        "command": exe,
        "args": ["mcp", "--bridge", socket.path],
    }}});
    let model = std::env::var("SCARLET_MODEL").ok().filter(|m| !m.trim().is_empty());
    let mut command = Command::new("claude");
    command
        .args(["-p", "--output-format", "stream-json", "--verbose"])
        .args(["--model", model.as_deref().unwrap_or(DEFAULT_MODEL)])
        // None of the user's own settings, hooks, plugins, MCP servers or built-in tools: only ours.
        .args(["--setting-sources", "", "--strict-mcp-config", "--tools", ""])
        .args(["--mcp-config", &servers.to_string(), "--allowedTools", "mcp__scarlet"])
        .args(["--system-prompt", SYSTEM_PROMPT]);
    command.args([if first { "--session-id" } else { "--resume" }, conversation]);
    // Its own folder, so these conversations stay out of the user's projects.
    if let Some(dir) = dirs::cache_dir().map(|d| d.join("scarlet").join("agent")) {
        let _ = std::fs::create_dir_all(&dir);
        command.current_dir(dir);
    }
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let mut child = command.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => {
            "Ask needs Claude Code. Install it from claude.com/code, then run claude once to sign in.".to_owned()
        }
        _ => format!("Could not start Claude Code: {e}"),
    })?;
    let mut stdin = child.stdin.take().expect("piped");
    stdin.write_all(prompt.as_bytes()).await.map_err(|e| e.to_string())?;
    drop(stdin);
    let mut stdout = BufReader::new(child.stdout.take().expect("piped")).lines();
    let mut stderr = child.stderr.take().expect("piped");
    if !first {
        events(Event::Activity("Thinking".into()));
    }

    // Answered tool calls; dropped (and so stopped) with this task.
    let mut bridges = JoinSet::new();
    let mut finished = None;
    loop {
        tokio::select! {
            accepted = socket.listener.accept() => {
                if let Ok((stream, _)) = accepted {
                    bridges.spawn(bridge(stream, session.clone(), events.clone()));
                }
            }
            line = stdout.next_line() => match line {
                Ok(Some(line)) => {
                    let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
                    if message["type"] == "result" {
                        let text = message["result"].as_str().unwrap_or_default().trim().to_owned();
                        finished = Some((message["is_error"] == true, text));
                    } else if message["type"] == "assistant"
                        && message["message"]["content"].as_array().is_some_and(|c| c.iter().any(|b| b["type"] == "text"))
                        && finished.is_none()
                    {
                        events(Event::Activity("Thinking".into()));
                    }
                }
                _ => break,
            },
        }
    }
    let status = child.wait().await.map_err(|e| e.to_string())?;
    match finished {
        Some((false, text)) => {
            events(Event::Finished { text });
            Ok(())
        }
        Some((true, text)) if !text.is_empty() => Err(text),
        _ => {
            let mut output = String::new();
            let _ = stderr.read_to_string(&mut output).await;
            let last = output.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or_default().trim().to_owned();
            if last.is_empty() { Err(format!("Claude Code stopped ({status})")) } else { Err(last) }
        }
    }
}

/// Answers one `scarlet mcp --bridge` process's tool calls.
async fn bridge(stream: UnixStream, session: Arc<Session>, events: Arc<dyn Fn(Event) + Send + Sync>) {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(request) = serde_json::from_str::<Value>(&line) else { break };
        let name = request["name"].as_str().unwrap_or_default();
        let arguments = &request["arguments"];
        events(Event::Activity(tools::activity(name, arguments)));
        let result = if name == tools::SHOW_RESULT {
            show_result(&session, arguments, &*events).await
        } else {
            tools::call(&session, name, arguments).await
        };
        let reply = mcp::reply(result);
        if write.write_all(format!("{reply}\n").as_bytes()).await.is_err() {
            break;
        }
    }
}

/// Runs the answer for the user. Failures go back to the agent to fix.
async fn show_result(
    session: &Session,
    arguments: &Value,
    events: &(dyn Fn(Event) + Send + Sync),
) -> Result<String, String> {
    let sql = arguments["sql"].as_str().ok_or("`sql` is required")?.trim().trim_end_matches(';').trim().to_owned();
    let title = arguments["title"].as_str().unwrap_or_default().trim().to_owned();
    let outcome = session.peek(&sql, RESULT_LIMIT).await.map_err(|e| e.message)?;
    if outcome.columns.is_none() {
        return Err("show_result needs a statement that returns rows.".into());
    }
    let preview = tools::rows_as_text(&outcome, 10);
    events(Event::Answer { sql, title, outcome });
    Ok(format!("Shown to the user. The first rows:\n{preview}"))
}

/// The bridge's listening socket, removed again when dropped.
struct Socket {
    listener: UnixListener,
    path: String,
}

impl Socket {
    fn bind() -> Result<Socket, String> {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let name = format!("scarlet-{}-{}.sock", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
        let path: PathBuf = std::env::temp_dir().join(name);
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).map_err(|e| format!("Could not open the agent bridge: {e}"))?;
        restrict(&path);
        Ok(Socket { listener, path: path.to_string_lossy().into_owned() })
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Only this user may connect to the socket.
fn restrict(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

/// A random UUID (version 4) for a new conversation.
fn new_session_id() -> String {
    use std::hash::{BuildHasher, RandomState};
    let a = RandomState::new().hash_one(std::time::SystemTime::now());
    let b = RandomState::new().hash_one(std::process::id());
    let hex = format!("{a:016x}{b:016x}");
    format!(
        "{}-{}-4{}-{}{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[13..16],
        ["8", "9", "a", "b"][(b & 3) as usize],
        &hex[17..20],
        &hex[20..32]
    )
}
