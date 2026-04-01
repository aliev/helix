pub mod actions;
pub mod cli;
pub mod ipc;
pub mod mcp;
pub mod params;

pub use ipc::{
    send_command, send_command_with_args, start_server, CurrentDocumentSnapshot, DiagnosticSnapshot,
    IpcRequest, IpcResponse, IpcServer, LayoutSnapshot, OpenDocumentSnapshot, RemoteCommand,
    SelectionSnapshot, ViewLayoutSnapshot,
};
pub use params::{
    FocusSplitArgs, GetCurrentDocumentArgs, GetDiagnosticsArgs, GetSelectionsArgs,
    GotoLocationArgs, McpPresenceArgs, OpenFileArgs, ReplaceSelectionArgs, SelectLinesArgs, SplitDirection,
    SplitOpenArgs,
};
