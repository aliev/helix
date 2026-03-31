use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
};
use serde_json::{Map, Value};
use anyhow::{bail, Result};

pub fn looks_like_socket_path(value: &str) -> bool {
    value.ends_with(".sock")
}

pub fn default_socket_path(working_directory: Option<&Path>) -> PathBuf {
    let base = working_directory
        .map(Path::to_path_buf)
        .unwrap_or_else(helix_stdx::env::current_working_dir);
    let canonical_base = helix_stdx::path::canonicalize(&base);
    let project_name = base
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(sanitize_socket_name)
        .unwrap_or_else(|| "hx".to_string());
    let mut hasher = DefaultHasher::new();
    canonical_base.hash(&mut hasher);
    let hash = hasher.finish();

    PathBuf::from("/tmp").join(format!("{project_name}-{hash:016x}.sock"))
}

pub fn is_supported_remote_command(command: &str) -> bool {
    matches!(
        command,
        "get-active-context"
            | "get-layout"
            | "reload-all"
            | "get-current-document"
            | "get-open-documents"
            | "get-selections"
            | "open-file"
            | "split-open"
            | "focus-split"
            | "close-split"
            | "goto-location"
            | "select-lines"
            | "get-diagnostics"
    )
}

pub fn parse_remote_arguments(command: &str, raw_args: &[String]) -> Result<Option<Value>> {
    if raw_args.is_empty() {
        return Ok(None);
    }

    let mut object = Map::new();

    if matches!(command, "open-file" | "split-open") && !raw_args[0].contains('=') {
        object.insert("path".into(), Value::String(raw_args[0].clone()));
        for arg in &raw_args[1..] {
            let (key, value) = parse_remote_argument(arg)?;
            object.insert(key, Value::String(value));
        }
        return Ok(Some(Value::Object(object)));
    }

    for arg in raw_args {
        let (key, value) = parse_remote_argument(arg)?;
        object.insert(key, Value::String(value));
    }

    Ok(Some(Value::Object(object)))
}

fn sanitize_socket_name(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect();

    if sanitized.is_empty() {
        "hx".to_string()
    } else {
        sanitized
    }
}

fn parse_remote_argument(arg: &str) -> Result<(String, String)> {
    let Some((key, value)) = arg.split_once('=') else {
        bail!("remote argument '{arg}' must be in key=value form");
    };
    if key.is_empty() {
        bail!("remote argument '{arg}' has an empty key");
    }
    Ok((key.to_string(), value.to_string()))
}
