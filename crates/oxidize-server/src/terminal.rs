use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

const DEFAULT_COLS: u16 = 100;
const DEFAULT_ROWS: u16 = 28;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    Input { data: String },
    Resize { cols: u16, rows: u16 },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage {
    Output { data: String },
    Error { message: String },
    Exit { code: Option<i32> },
}

pub async fn handle_socket(socket: WebSocket, cwd: PathBuf) {
    if let Err(error) = run_terminal(socket, cwd).await {
        tracing::warn!("terminal session failed: {error}");
    }
}

async fn run_terminal(socket: WebSocket, cwd: PathBuf) -> anyhow::Result<()> {
    let pair = native_pty_system().openpty(PtySize {
        rows: DEFAULT_ROWS,
        cols: DEFAULT_COLS,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut command = CommandBuilder::new(shell_command());
    command.cwd(cwd);
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    let mut child = pair.slave.spawn_command(command)?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader()?;
    let writer = Arc::new(Mutex::new(pair.master.take_writer()?));
    let master = Arc::new(Mutex::new(pair.master));
    let (pty_tx, mut pty_rx) = mpsc::unbounded_channel::<ServerMessage>();

    let output_tx = pty_tx.clone();
    std::thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        let mut pending = Vec::new();
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => {
                    if !pending.is_empty() {
                        let _ = output_tx.send(ServerMessage::Output {
                            data: String::from_utf8_lossy(&pending).into_owned(),
                        });
                    }
                    break;
                }
                Ok(count) => {
                    pending.extend_from_slice(&buffer[..count]);
                    if let Some(data) = take_valid_utf8(&mut pending)
                        && output_tx.send(ServerMessage::Output { data }).is_err()
                    {
                        break;
                    }
                }
                Err(error) => {
                    let _ = output_tx.send(ServerMessage::Error {
                        message: error.to_string(),
                    });
                    break;
                }
            }
        }
    });

    std::thread::spawn(move || {
        let status = child.wait().ok();
        let _ = pty_tx.send(ServerMessage::Exit {
            code: status.map(|status| status.exit_code() as i32),
        });
    });

    let (mut ws_sender, mut ws_receiver) = socket.split();
    loop {
        tokio::select! {
            Some(message) = pty_rx.recv() => {
                let payload = serde_json::to_string(&message)?;
                if ws_sender.send(Message::Text(payload.into())).await.is_err() { break; }
            }
            Some(message) = ws_receiver.next() => {
                let Ok(message) = message else { break };
                if !handle_client_message(message, &writer, &master) { break; }
            }
            else => break,
        }
    }
    Ok(())
}

fn handle_client_message(
    message: Message,
    writer: &Arc<Mutex<Box<dyn Write + Send>>>,
    master: &Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
) -> bool {
    match message {
        Message::Text(text) => {
            let Ok(message) = serde_json::from_str::<ClientMessage>(&text) else {
                return true;
            };
            match message {
                ClientMessage::Input { data } => {
                    if let Ok(mut writer) = writer.lock() {
                        let _ = writer.write_all(data.as_bytes());
                        let _ = writer.flush();
                    }
                }
                ClientMessage::Resize { cols, rows } => {
                    if let Ok(master) = master.lock() {
                        let _ = master.resize(PtySize {
                            rows: rows.max(1),
                            cols: cols.max(1),
                            pixel_width: 0,
                            pixel_height: 0,
                        });
                    }
                }
            }
            true
        }
        Message::Binary(data) => {
            if let Ok(mut writer) = writer.lock() {
                let _ = writer.write_all(&data);
                let _ = writer.flush();
            }
            true
        }
        Message::Close(_) => false,
        Message::Ping(_) | Message::Pong(_) => true,
    }
}

fn take_valid_utf8(bytes: &mut Vec<u8>) -> Option<String> {
    match std::str::from_utf8(bytes) {
        Ok(text) if !text.is_empty() => {
            let text = text.to_owned();
            bytes.clear();
            Some(text)
        }
        Ok(_) => None,
        Err(error) => {
            let valid = error.valid_up_to();
            if error.error_len().is_none() && valid == 0 {
                return None;
            }
            let end = valid + error.error_len().unwrap_or(0);
            let text = String::from_utf8_lossy(&bytes[..end]).into_owned();
            bytes.drain(..end);
            (!text.is_empty()).then_some(text)
        }
    }
}

fn shell_command() -> String {
    if cfg!(windows) {
        return "powershell.exe".to_owned();
    }
    std::env::var("OXIDIZE_SHELL")
        .ok()
        .filter(|shell| !shell.trim().is_empty())
        .or_else(|| {
            ["/bin/bash", "/usr/bin/bash"]
                .into_iter()
                .find(|path| Path::new(path).exists())
                .map(str::to_owned)
        })
        .or_else(|| std::env::var("SHELL").ok())
        .unwrap_or_else(|| "/bin/sh".to_owned())
}
