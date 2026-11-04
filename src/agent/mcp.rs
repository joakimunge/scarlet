//! `scarlet mcp`: a Model Context Protocol server on stdin/stdout.
//!
//! On its own it connects to a database and serves the read-only tools, so any
//! MCP client can explore it. With `--bridge` it is the go-between for Scarlet's
//! Ask: tool calls are passed to the running Scarlet over a Unix socket, which
//! answers them on its own connection.

use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

use super::{
    mask::{Masker, Visibility},
    tools,
};
use crate::db::postgres::Session;

pub enum Backend {
    Direct(Arc<Session>),
    Bridge(PathBuf),
}

/// A tool call passed over the bridge: one JSON line `{"name", "arguments"}`
/// each way, answered by `{"text", "is_error"}`.
pub fn reply(result: Result<String, String>) -> Value {
    match result {
        Ok(text) => json!({"text": text, "is_error": false}),
        Err(text) => json!({"text": text, "is_error": true}),
    }
}

/// Serves MCP on stdin/stdout. `visibility` decides how much of the data the
/// client may see; with the bridge, Scarlet enforces it again on its side.
pub async fn serve(backend: Backend, visibility: Visibility) -> Result<()> {
    let masker = std::sync::Mutex::new(Masker::default());
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    let mut bridge = match &backend {
        Backend::Bridge(path) => {
            let stream =
                UnixStream::connect(path).await.with_context(|| format!("connecting to {}", path.display()))?;
            let (read, write) = stream.into_split();
            Some((BufReader::new(read).lines(), write))
        }
        Backend::Direct(_) => None,
    };
    while let Some(line) = lines.next_line().await? {
        let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
        // Notifications have no id and get no reply.
        let Some(id) = message.get("id").cloned() else { continue };
        let params = &message["params"];
        let result = match message["method"].as_str().unwrap_or_default() {
            "initialize" => Ok(json!({
                "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "scarlet", "version": env!("CARGO_PKG_VERSION")},
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tools::definitions(bridge.is_some(), visibility)})),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or_default().to_owned();
                let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
                let answer = match (&backend, &mut bridge) {
                    (_, Some((replies, write))) => {
                        let request = json!({"name": name, "arguments": arguments});
                        write.write_all(format!("{request}\n").as_bytes()).await?;
                        let line = replies.next_line().await?.context("Scarlet closed the bridge")?;
                        serde_json::from_str(&line)?
                    }
                    (Backend::Direct(session), None) => {
                        reply(tools::call(session, &name, &arguments, visibility, &masker).await)
                    }
                    (Backend::Bridge(_), None) => unreachable!("the bridge connects first"),
                };
                Ok(json!({"content": [{"type": "text", "text": answer["text"]}], "isError": answer["is_error"]}))
            }
            method => Err(json!({"code": -32601, "message": format!("Unknown method {method}")})),
        };
        let response = match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
        };
        stdout.write_all(format!("{response}\n").as_bytes()).await?;
        stdout.flush().await?;
    }
    Ok(())
}
