use anyhow::{Context, Result};
use helix_loader::VERSION_AND_GIT_HASH;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use tokio::time::{sleep, Duration};
use tokio::io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};

use crate::remote::RemoteCommand;

const MCP_PROTOCOL_VERSION: &str = "2025-11-25";

pub async fn run_stdio(socket_path: PathBuf) -> Result<i32> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut lines = BufReader::new(stdin).lines();
    let mut writer = BufWriter::new(stdout);
    let mut initialized = false;
    let mut heartbeat_started = false;

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }

        let value: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(err) => {
                write_json(
                    &mut writer,
                    &jsonrpc_error(Value::Null, -32700, format!("parse error: {err}")),
                )
                .await?;
                continue;
            }
        };

        if let Some(items) = value.as_array() {
            for item in items {
                if let Some(response) =
                    handle_message(
                        item.clone(),
                        &socket_path,
                        &mut initialized,
                        &mut heartbeat_started,
                    )
                    .await?
                {
                    write_json(&mut writer, &response).await?;
                }
            }
            continue;
        }

        if let Some(response) =
            handle_message(value, &socket_path, &mut initialized, &mut heartbeat_started).await?
        {
            write_json(&mut writer, &response).await?;
        }
    }

    writer.flush().await?;
    Ok(0)
}

async fn handle_message(
    message: Value,
    socket_path: &PathBuf,
    initialized: &mut bool,
    heartbeat_started: &mut bool,
) -> Result<Option<Value>> {
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return Ok(None);
    };
    let id = message.get("id").cloned();

    match method {
        "initialize" => {
            let id = id.unwrap_or(Value::Null);
            let params: InitializeParams = serde_json::from_value(
                message.get("params").cloned().unwrap_or_else(|| json!({})),
            )
            .context("invalid initialize params")?;
            *initialized = true;

            let client_name = params
                .client_info
                .as_ref()
                .map(|info| info.name.clone())
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| "MCP".to_string());

            let client_id = format!("mcp-{}", std::process::id());
            let _ = send_presence_update(socket_path, &client_id, &client_name).await;
            if !*heartbeat_started {
                *heartbeat_started = true;
                tokio::spawn(mcp_presence_heartbeat(
                    socket_path.clone(),
                    client_id.clone(),
                    client_name.clone(),
                ));
            }

            Ok(Some(jsonrpc_result(
                id,
                json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {
                        "tools": {
                            "listChanged": false
                        }
                    },
                    "serverInfo": {
                        "name": "hx-mcp",
                        "title": "Helix MCP Bridge",
                        "version": VERSION_AND_GIT_HASH,
                    },
                    "instructions": "Connect this bridge to a running Helix instance started with --listen. Use the tools to inspect the live editor state or trigger reload-all."
                }),
            )))
        }
        "notifications/initialized" => Ok(None),
        "ping" => Ok(id.map(|id| jsonrpc_result(id, json!({})))),
        "tools/list" => {
            let id = id.unwrap_or(Value::Null);
            Ok(Some(jsonrpc_result(
                id,
                json!({
                    "tools": [
                        tool(
                            "reload_all",
                            "Reload every open document from disk in the running Helix session. Call this after external file edits so Helix reflects on-disk changes.",
                            json!({
                                "type": "object",
                                "properties": {},
                                "additionalProperties": false
                            }),
                        ),
                        tool(
                            "get_current_document",
                            "Read the active document from the running Helix session, including its path, language, full text, and selection metadata.",
                            json!({
                                "type": "object",
                                "properties": {},
                                "additionalProperties": false
                            }),
                        ),
                        tool(
                            "get_open_documents",
                            "List the open documents in the running Helix session, including which one is currently focused.",
                            json!({
                                "type": "object",
                                "properties": {},
                                "additionalProperties": false
                            }),
                        ),
                        tool(
                            "get_selections",
                            "Read the current selection ranges from the active Helix document. Useful before opening or navigating elsewhere.",
                            json!({
                                "type": "object",
                                "properties": {},
                                "additionalProperties": false
                            }),
                        ),
                        tool(
                            "open_file",
                            "Open a file in the running Helix session and optionally jump to a 1-based line and column. Use this to move the user's editor to a file you want them to see.",
                            json!({
                                "type": "object",
                                "properties": {
                                    "path": { "type": "string" },
                                    "line": { "type": "integer", "minimum": 1 },
                                    "column": { "type": "integer", "minimum": 1 }
                                },
                                "required": ["path"],
                                "additionalProperties": false
                            }),
                        ),
                        tool(
                            "goto_location",
                            "Move the cursor in the running Helix session to a 1-based line and optional column. If path is provided, open that file first.",
                            json!({
                                "type": "object",
                                "properties": {
                                    "path": { "type": "string" },
                                    "line": { "type": "integer", "minimum": 1 },
                                    "column": { "type": "integer", "minimum": 1 }
                                },
                                "required": ["line"],
                                "additionalProperties": false
                            }),
                        ),
                        tool(
                            "select_lines",
                            "Select a 1-based inclusive line range in the running Helix session. If path is provided, open that file first. Useful when the user asks you to highlight or select code.",
                            json!({
                                "type": "object",
                                "properties": {
                                    "path": { "type": "string" },
                                    "start_line": { "type": "integer", "minimum": 1 },
                                    "end_line": { "type": "integer", "minimum": 1 }
                                },
                                "required": ["start_line"],
                                "additionalProperties": false
                            }),
                        ),
                        tool(
                            "get_diagnostics",
                            "Read diagnostics for the active Helix document after the editor has reloaded or re-analyzed it. If files were edited outside Helix, call reload_all first.",
                            json!({
                                "type": "object",
                                "properties": {},
                                "additionalProperties": false
                            }),
                        )
                    ]
                }),
            )))
        }
        "tools/call" => {
            let id = id.unwrap_or(Value::Null);
            if !*initialized {
                return Ok(Some(jsonrpc_error(
                    id,
                    -32002,
                    "server not initialized".to_string(),
                )));
            }

            let params: ToolCallParams = serde_json::from_value(
                message.get("params").cloned().unwrap_or_else(|| json!({})),
            )
            .context("invalid tools/call params")?;

            let remote = match params.name.as_str() {
                "reload_all" => RemoteCommand::ReloadAll,
                "get_current_document" => RemoteCommand::GetCurrentDocument,
                "get_open_documents" => RemoteCommand::GetOpenDocuments,
                "get_selections" => RemoteCommand::GetSelections,
                "open_file" => RemoteCommand::OpenFile,
                "goto_location" => RemoteCommand::GotoLocation,
                "select_lines" => RemoteCommand::SelectLines,
                "get_diagnostics" => RemoteCommand::GetDiagnostics,
                _ => {
                    return Ok(Some(jsonrpc_error(
                        id,
                        -32601,
                        format!("unknown tool: {}", params.name),
                    )));
                }
            };

            let response = match send_remote_command(socket_path, remote, params.arguments).await {
                Ok(response) => response,
                Err(err) => {
                    return Ok(Some(jsonrpc_result(
                        id,
                        json!({
                            "content": [
                                {
                                    "type": "text",
                                    "text": format!("tool error: {err}"),
                                }
                            ],
                            "isError": true,
                        }),
                    )));
                }
            };
            let summary = if response.ok {
                response.message.clone()
            } else {
                format!("tool error: {}", response.message)
            };
            let mut result = json!({
                "content": [
                    {
                        "type": "text",
                        "text": summary,
                    }
                ],
                "isError": !response.ok,
            });
            if let Some(data) = response.data {
                result["structuredContent"] = match data {
                    Value::Object(map) => Value::Object(map),
                    other => json!({ "result": other }),
                };
            }

            Ok(Some(jsonrpc_result(id, result)))
        }
        _ => Ok(id.map(|id| jsonrpc_error(id, -32601, format!("method not found: {method}")))),
    }
}

async fn mcp_presence_heartbeat(socket_path: PathBuf, client_id: String, client_name: String) {
    loop {
        sleep(Duration::from_secs(10)).await;
        let _ = send_presence_update(&socket_path, &client_id, &client_name).await;
    }
}

async fn send_presence_update(socket_path: &PathBuf, client_id: &str, client_name: &str) -> Result<()> {
    let _ = crate::remote::send_command_with_args(
        socket_path,
        RemoteCommand::UpdateMcpPresence,
        Some(json!({
            "client_id": client_id,
            "client_name": client_name,
        })),
    )
    .await?;
    Ok(())
}

async fn send_remote_command(
    socket_path: &PathBuf,
    remote: RemoteCommand,
    arguments: Option<Value>,
) -> Result<crate::remote::IpcResponse> {
    let mut last_err = None;

    for delay_ms in [0_u64, 150, 400] {
        if delay_ms > 0 {
            sleep(Duration::from_millis(delay_ms)).await;
        }

        match crate::remote::send_command_with_args(socket_path, remote, arguments.clone()).await {
            Ok(response) => return Ok(response),
            Err(err) => last_err = Some(err),
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("failed to contact Helix session")))
}

fn jsonrpc_result(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

fn jsonrpc_error(id: Value, code: i64, message: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message,
        }
    })
}

fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": input_schema,
    })
}

async fn write_json(writer: &mut BufWriter<io::Stdout>, value: &Value) -> Result<()> {
    let payload = serde_json::to_vec(value)?;
    writer.write_all(&payload).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

#[derive(Debug, Deserialize)]
struct InitializeParams {
    #[serde(rename = "protocolVersion")]
    _protocol_version: String,
    #[serde(rename = "clientInfo")]
    client_info: Option<ClientInfo>,
}

#[derive(Debug, Deserialize)]
struct ClientInfo {
    name: String,
    #[serde(rename = "version")]
    _version: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ToolCallParams {
    name: String,
    #[serde(default)]
    arguments: Option<Value>,
}
