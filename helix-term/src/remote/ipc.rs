use anyhow::{bail, Context, Result};
use helix_core::Selection;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RemoteCommand {
    ReloadAll,
    GetCurrentDocument,
    GetOpenDocuments,
    GetSelections,
    OpenFile,
    GotoLocation,
    SelectLines,
    GetDiagnostics,
    UpdateMcpPresence,
}

impl RemoteCommand {
    pub fn parse(command: &str) -> Result<Self> {
        match command {
            "reload-all" => Ok(Self::ReloadAll),
            "get-current-document" => Ok(Self::GetCurrentDocument),
            "get-open-documents" => Ok(Self::GetOpenDocuments),
            "get-selections" => Ok(Self::GetSelections),
            "open-file" => Ok(Self::OpenFile),
            "goto-location" => Ok(Self::GotoLocation),
            "select-lines" => Ok(Self::SelectLines),
            "get-diagnostics" => Ok(Self::GetDiagnostics),
            "update-mcp-presence" => Ok(Self::UpdateMcpPresence),
            other => bail!("unsupported remote command: {other}"),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReloadAll => "reload-all",
            Self::GetCurrentDocument => "get-current-document",
            Self::GetOpenDocuments => "get-open-documents",
            Self::GetSelections => "get-selections",
            Self::OpenFile => "open-file",
            Self::GotoLocation => "goto-location",
            Self::SelectLines => "select-lines",
            Self::GetDiagnostics => "get-diagnostics",
            Self::UpdateMcpPresence => "update-mcp-presence",
        }
    }
}

#[derive(Debug)]
pub struct IpcRequest {
    pub command: RemoteCommand,
    pub arguments: Option<Value>,
    pub reply: oneshot::Sender<IpcResponse>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcResponse {
    pub ok: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl IpcResponse {
    pub fn ok(message: impl Into<String>) -> Self {
        Self {
            ok: true,
            message: message.into(),
            data: None,
        }
    }

    pub fn err(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            message: message.into(),
            data: None,
        }
    }

    pub fn ok_with_data(message: impl Into<String>, data: Value) -> Self {
        Self {
            ok: true,
            message: message.into(),
            data: Some(data),
        }
    }
}

#[derive(Debug, Deserialize)]
struct IpcClientRequest {
    command: RemoteCommand,
    #[serde(default)]
    arguments: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SelectionSnapshot {
    pub anchor_line: usize,
    pub head_line: usize,
    pub start_line: usize,
    pub end_line: usize,
}

impl SelectionSnapshot {
    pub fn from_selection(selection: &Selection, text: helix_core::RopeSlice<'_>) -> Vec<Self> {
        selection
            .iter()
            .map(|range| {
                let start = range.from();
                let end = range.to();
                Self {
                    anchor_line: text.char_to_line(range.anchor) + 1,
                    head_line: text.char_to_line(range.head) + 1,
                    start_line: text.char_to_line(start) + 1,
                    end_line: text.char_to_line(end) + 1,
                }
            })
            .collect()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CurrentDocumentSnapshot {
    pub path: Option<String>,
    pub relative_path: Option<String>,
    pub language: Option<String>,
    pub modified: bool,
    pub line_count: usize,
    pub selections: Vec<SelectionSnapshot>,
    pub primary_selection_text: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct OpenDocumentSnapshot {
    pub path: Option<String>,
    pub relative_path: Option<String>,
    pub modified: bool,
    pub is_current: bool,
    pub line_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticSnapshot {
    pub path: Option<String>,
    pub line: usize,
    pub end_line: usize,
    pub message: String,
    pub severity: Option<String>,
    pub source: Option<String>,
}

pub struct IpcServer {
    path: PathBuf,
    receiver: mpsc::UnboundedReceiver<IpcRequest>,
    task: tokio::task::JoinHandle<()>,
}

impl IpcServer {
    pub async fn recv(&mut self) -> Option<IpcRequest> {
        self.receiver.recv().await
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn shutdown(self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
pub fn start_server(path: PathBuf) -> Result<IpcServer> {
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::UnixListener,
    };

    use std::os::unix::net::UnixListener as StdUnixListener;

    if path.exists() {
        std::fs::remove_file(&path).with_context(|| {
            format!("failed to remove existing IPC socket at {}", path.display())
        })?;
    }

    let listener = StdUnixListener::bind(&path)
        .with_context(|| format!("failed to bind IPC socket at {}", path.display()))?;
    listener
        .set_nonblocking(true)
        .with_context(|| format!("failed to set IPC socket nonblocking at {}", path.display()))?;
    let listener = UnixListener::from_std(listener)
        .with_context(|| format!("failed to initialize tokio IPC socket at {}", path.display()))?;
    let (sender, receiver) = mpsc::unbounded_channel();
    let socket_path = path.clone();

    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let sender = sender.clone();

            tokio::spawn(async move {
                let (reader, mut writer) = stream.into_split();
                let mut reader = BufReader::new(reader);
                let mut line = String::new();
                let response = match reader.read_line(&mut line).await {
                    Ok(0) => IpcResponse::err("empty IPC request"),
                    Ok(_) => match serde_json::from_str::<IpcClientRequest>(&line) {
                        Ok(request) => {
                            let (reply_tx, reply_rx) = oneshot::channel();
                            if sender
                                .send(IpcRequest {
                                    command: request.command,
                                    arguments: request.arguments,
                                    reply: reply_tx,
                                })
                                .is_err()
                            {
                                IpcResponse::err("helix IPC server is shutting down")
                            } else {
                                reply_rx.await.unwrap_or_else(|_| {
                                    IpcResponse::err("helix IPC request was cancelled")
                                })
                            }
                        }
                        Err(err) => IpcResponse::err(format!("invalid IPC request: {err}")),
                    },
                    Err(err) => IpcResponse::err(format!("failed to read IPC request: {err}")),
                };

                if let Ok(mut payload) = serde_json::to_vec(&response) {
                    payload.push(b'\n');
                    let _ = writer.write_all(&payload).await;
                    let _ = writer.shutdown().await;
                }
            });
        }
    });

    Ok(IpcServer {
        path: socket_path,
        receiver,
        task,
    })
}

#[cfg(not(unix))]
pub fn start_server(_path: PathBuf) -> Result<IpcServer> {
    bail!("helix IPC is currently supported only on unix platforms")
}

#[cfg(unix)]
pub async fn send_command(path: &Path, command: RemoteCommand) -> Result<IpcResponse> {
    send_command_with_args(path, command, None).await
}

#[cfg(unix)]
pub async fn send_command_with_args(
    path: &Path,
    command: RemoteCommand,
    arguments: Option<Value>,
) -> Result<IpcResponse> {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::UnixStream,
    };

    let mut stream = UnixStream::connect(path)
        .await
        .with_context(|| format!("failed to connect to IPC socket {}", path.display()))?;
    let payload = serde_json::to_vec(&serde_json::json!({
        "command": command.as_str(),
        "arguments": arguments,
    }))?;
    stream.write_all(&payload).await?;
    stream.write_all(b"\n").await?;
    stream.shutdown().await?;

    let mut response = Vec::new();
    stream.read_to_end(&mut response).await?;
    let response: IpcResponse = serde_json::from_slice(&response)
        .with_context(|| format!("invalid IPC response from {}", path.display()))?;
    Ok(response)
}

#[cfg(not(unix))]
pub async fn send_command(_path: &Path, _command: RemoteCommand) -> Result<IpcResponse> {
    send_command_with_args(_path, _command, None).await
}

#[cfg(not(unix))]
pub async fn send_command_with_args(
    _path: &Path,
    _command: RemoteCommand,
    _arguments: Option<Value>,
) -> Result<IpcResponse> {
    Err(anyhow::anyhow!(
        "helix IPC is currently supported only on unix platforms"
    ))
}
