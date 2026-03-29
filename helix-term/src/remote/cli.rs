use std::path::{Path, PathBuf};

pub fn looks_like_socket_path(value: &str) -> bool {
    value.ends_with(".sock")
}

pub fn default_socket_path(working_directory: Option<&Path>) -> PathBuf {
    let base = working_directory
        .map(Path::to_path_buf)
        .unwrap_or_else(helix_stdx::env::current_working_dir);
    let project_name = base
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(sanitize_socket_name)
        .unwrap_or_else(|| "hx".to_string());

    PathBuf::from("/tmp").join(format!("{project_name}.sock"))
}

pub fn is_supported_remote_command(command: &str) -> bool {
    matches!(
        command,
        "reload-all"
            | "get-current-document"
            | "get-open-documents"
            | "get-selections"
            | "open-file"
            | "goto-location"
            | "select-lines"
            | "get-diagnostics"
    )
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
