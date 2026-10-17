use std::path::PathBuf;

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};

#[derive(Debug, Deserialize)]
pub struct LanguageServerQuery {
    pub root: String,
    pub initial_file: Option<String>,
    pub language: String,
}

#[derive(Debug, Clone)]
pub struct LanguageServerKind {
    command: String,
    args: Vec<String>,
}

impl LanguageServerKind {
    pub fn from_language(language: &str) -> Option<Self> {
        let environment_key = format!(
            "OXIDIZE_LSP_{}",
            language.to_ascii_uppercase().replace(['-', '.'], "_")
        );
        if let Ok(value) = std::env::var(environment_key)
            && let Some(server) = Self::from_command_line(&value)
        {
            return Some(server);
        }
        match language {
            "rust" => Some(Self::new("rust-analyzer", &[])),
            _ => None,
        }
    }

    fn new(command: &str, args: &[&str]) -> Self {
        Self {
            command: command.to_owned(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        }
    }

    fn from_command_line(value: &str) -> Option<Self> {
        let mut parts = value.split_whitespace();
        Some(Self {
            command: parts.next()?.to_owned(),
            args: parts.map(str::to_owned).collect(),
        })
    }
}

pub async fn handle_socket(socket: WebSocket, cwd: PathBuf, kind: LanguageServerKind) {
    if let Err(error) = run_language_server(socket, cwd, kind).await {
        tracing::warn!("language server session failed: {error}");
    }
}

async fn run_language_server(
    socket: WebSocket,
    cwd: PathBuf,
    kind: LanguageServerKind,
) -> anyhow::Result<()> {
    let display_command = kind.command.clone();
    let mut child = Command::new(&kind.command)
        .args(&kind.args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| anyhow::anyhow!("failed to start {display_command}: {error}"))?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("language server stdin is unavailable"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("language server stdout is unavailable"))?;

    if let Some(mut stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut buffer = [0_u8; 4096];
            loop {
                match stderr.read(&mut buffer).await {
                    Ok(0) => break,
                    Ok(count) => tracing::debug!(
                        "language server: {}",
                        String::from_utf8_lossy(&buffer[..count]).trim_end()
                    ),
                    Err(_) => break,
                }
            }
        });
    }

    let (mut ws_sender, mut ws_receiver) = socket.split();
    let mut stdout_buffer = Vec::new();
    let mut read_buffer = [0_u8; 8192];

    loop {
        tokio::select! {
            result = stdout.read(&mut read_buffer) => {
                let count = result?;
                if count == 0 { break; }
                stdout_buffer.extend_from_slice(&read_buffer[..count]);
                while let Some(payload) = take_lsp_payload(&mut stdout_buffer)? {
                    if ws_sender.send(Message::Text(payload.into())).await.is_err() {
                        break;
                    }
                }
            }
            Some(message) = ws_receiver.next() => {
                let message = message?;
                if !write_client_message(message, &mut stdin).await? {
                    break;
                }
            }
            else => break,
        }
    }

    let _ = child.kill().await;
    Ok(())
}

async fn write_client_message(
    message: Message,
    stdin: &mut tokio::process::ChildStdin,
) -> anyhow::Result<bool> {
    let payload = match message {
        Message::Text(text) => text.as_bytes().to_vec(),
        Message::Binary(bytes) => bytes.to_vec(),
        Message::Close(_) => return Ok(false),
        Message::Ping(_) | Message::Pong(_) => return Ok(true),
    };
    stdin
        .write_all(format!("Content-Length: {}\r\n\r\n", payload.len()).as_bytes())
        .await?;
    stdin.write_all(&payload).await?;
    stdin.flush().await?;
    Ok(true)
}

fn take_lsp_payload(buffer: &mut Vec<u8>) -> anyhow::Result<Option<String>> {
    const MAX_MESSAGE_BYTES: usize = 32 * 1024 * 1024;

    if buffer.len() > MAX_MESSAGE_BYTES + 64 * 1024 {
        anyhow::bail!("language server response buffer is too large");
    }
    let (header_end, delimiter_size) = if let Some(index) = find_subsequence(buffer, b"\r\n\r\n") {
        (index, 4)
    } else if let Some(index) = find_subsequence(buffer, b"\n\n") {
        (index, 2)
    } else {
        return Ok(None);
    };
    let headers = String::from_utf8_lossy(&buffer[..header_end]);
    let content_length = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().ok())
            .flatten()
    });
    let Some(content_length) = content_length else {
        anyhow::bail!("language server response is missing Content-Length");
    };
    if content_length > MAX_MESSAGE_BYTES {
        anyhow::bail!("language server response is too large");
    }
    let body_start = header_end + delimiter_size;
    let frame_end = body_start + content_length;
    if buffer.len() < frame_end {
        return Ok(None);
    }
    let payload = String::from_utf8_lossy(&buffer[body_start..frame_end]).into_owned();
    buffer.drain(..frame_end);
    Ok(Some(payload))
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
