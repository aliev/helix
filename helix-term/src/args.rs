use anyhow::Result;
use helix_core::Position;
use crate::remote::cli::{
    default_socket_path, is_supported_remote_command, looks_like_socket_path,
};
use helix_view::tree::Layout;
use indexmap::IndexMap;
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct Args {
    pub display_help: bool,
    pub display_version: bool,
    pub health: bool,
    pub health_arg: Option<String>,
    pub load_tutor: bool,
    pub fetch_grammars: bool,
    pub build_grammars: bool,
    pub split: Option<Layout>,
    pub verbosity: u64,
    pub log_file: Option<PathBuf>,
    pub config_file: Option<PathBuf>,
    pub files: IndexMap<PathBuf, Vec<Position>>,
    pub working_directory: Option<PathBuf>,
    pub ipc_listen_enabled: bool,
    pub ipc_listen: Option<PathBuf>,
    pub ipc_remote_enabled: bool,
    pub ipc_remote: Option<PathBuf>,
    pub ipc_remote_command: Option<String>,
    pub mcp_enabled: bool,
    pub mcp_socket: Option<PathBuf>,
}

impl Args {
    pub fn parse_args() -> Result<Args> {
        let mut args = Args::default();
        let mut argv = std::env::args().peekable();
        let mut line_number = 0;

        let mut insert_file_with_position = |file_with_position: &str| {
            let (filename, position) = parse_file(file_with_position);

            // Before setting the working directory, resolve all the paths in args.files
            let filename = helix_stdx::path::canonicalize(filename);

            args.files
                .entry(filename)
                .and_modify(|positions| positions.push(position))
                .or_insert_with(|| vec![position]);
        };

        argv.next(); // skip the program, we don't care about that

        while let Some(arg) = argv.next() {
            match arg.as_str() {
                "--" => break, // stop parsing at this point treat the remaining as files
                "--version" => args.display_version = true,
                "--help" => args.display_help = true,
                "--tutor" => args.load_tutor = true,
                "--vsplit" => match args.split {
                    Some(_) => anyhow::bail!("can only set a split once of a specific type"),
                    None => args.split = Some(Layout::Vertical),
                },
                "--hsplit" => match args.split {
                    Some(_) => anyhow::bail!("can only set a split once of a specific type"),
                    None => args.split = Some(Layout::Horizontal),
                },
                "--health" => {
                    args.health = true;
                    args.health_arg = argv.next_if(|opt| !opt.starts_with('-'));
                }
                "-g" | "--grammar" => match argv.next().as_deref() {
                    Some("fetch") => args.fetch_grammars = true,
                    Some("build") => args.build_grammars = true,
                    _ => {
                        anyhow::bail!("--grammar must be followed by either 'fetch' or 'build'")
                    }
                },
                "-c" | "--config" => match argv.next().as_deref() {
                    Some(path) => args.config_file = Some(path.into()),
                    None => anyhow::bail!("--config must specify a path to read"),
                },
                "--log" => match argv.next().as_deref() {
                    Some(path) => args.log_file = Some(path.into()),
                    None => anyhow::bail!("--log must specify a path to write"),
                },
                "--listen" => {
                    args.ipc_listen_enabled = true;
                    if let Some(path) = argv.next_if(|opt| looks_like_socket_path(opt)) {
                        args.ipc_listen = Some(path.into());
                    }
                }
                "--remote" => {
                    args.ipc_remote_enabled = true;
                    if let Some(path) = argv.next_if(|opt| looks_like_socket_path(opt)) {
                        args.ipc_remote = Some(path.into());
                    }
                    let command = match argv.next_if(|opt| !opt.starts_with('-')) {
                        Some(command) => command,
                        None => anyhow::bail!("--remote must be followed by a command"),
                    };
                    if !is_supported_remote_command(&command) {
                        anyhow::bail!("unsupported remote command {}", command);
                    }
                    args.ipc_remote_command = Some(command);
                }
                "--mcp" => {
                    args.mcp_enabled = true;
                    if let Some(path) = argv.next_if(|opt| looks_like_socket_path(opt)) {
                        args.mcp_socket = Some(path.into());
                    }
                }
                "-w" | "--working-dir" => match argv.next().as_deref() {
                    Some(path) => {
                        args.working_directory = if Path::new(path).is_dir() {
                            Some(PathBuf::from(path))
                        } else {
                            anyhow::bail!(
                                "--working-dir specified does not exist or is not a directory"
                            )
                        }
                    }
                    None => {
                        anyhow::bail!("--working-dir must specify an initial working directory")
                    }
                },
                arg if arg.starts_with("--") => {
                    anyhow::bail!("unexpected double dash argument: {}", arg)
                }
                arg if arg.starts_with('-') => {
                    let arg = arg.get(1..).unwrap().chars();
                    for chr in arg {
                        match chr {
                            'v' => args.verbosity += 1,
                            'V' => args.display_version = true,
                            'h' => args.display_help = true,
                            _ => anyhow::bail!("unexpected short arg {}", chr),
                        }
                    }
                }
                "+" => line_number = usize::MAX,
                arg if arg.starts_with('+') => {
                    match arg[1..].parse::<usize>() {
                        Ok(n) => line_number = n.saturating_sub(1),
                        _ => insert_file_with_position(arg),
                    };
                }
                arg => insert_file_with_position(arg),
            }
        }

        // push the remaining args, if any to the files
        for arg in argv {
            insert_file_with_position(&arg);
        }

        if line_number != 0 {
            if let Some(first_position) = args
                .files
                .first_mut()
                .and_then(|(_, positions)| positions.first_mut())
            {
                first_position.row = line_number;
            }
        }

        if args.ipc_listen_enabled && args.ipc_remote_enabled {
            anyhow::bail!("--listen and --remote cannot be used together");
        }

        if args.mcp_enabled && args.ipc_listen_enabled {
            anyhow::bail!("--mcp and --listen cannot be used together");
        }

        let default_socket = default_socket_path(args.working_directory.as_deref());
        if args.ipc_listen_enabled && args.ipc_listen.is_none() {
            args.ipc_listen = Some(default_socket.clone());
        }
        if args.ipc_remote_enabled && args.ipc_remote.is_none() {
            args.ipc_remote = Some(default_socket.clone());
        }
        if args.mcp_enabled && args.mcp_socket.is_none() {
            args.mcp_socket = Some(default_socket);
        }

        Ok(args)
    }
}

/// Parse arg into [`PathBuf`] and position.
pub(crate) fn parse_file(s: &str) -> (PathBuf, Position) {
    let def = || (PathBuf::from(s), Position::default());
    if Path::new(s).exists() {
        return def();
    }
    split_path_row_col(s)
        .or_else(|| split_path_row(s))
        .unwrap_or_else(def)
}

/// Split file.rs:10:2 into [`PathBuf`], row and col.
///
/// Does not validate if file.rs is a file or directory.
fn split_path_row_col(s: &str) -> Option<(PathBuf, Position)> {
    let mut s = s.trim_end_matches(':').rsplitn(3, ':');
    let col: usize = s.next()?.parse().ok()?;
    let row: usize = s.next()?.parse().ok()?;
    let path = s.next()?.into();
    let pos = Position::new(row.saturating_sub(1), col.saturating_sub(1));
    Some((path, pos))
}

/// Split file.rs:10 into [`PathBuf`] and row.
///
/// Does not validate if file.rs is a file or directory.
fn split_path_row(s: &str) -> Option<(PathBuf, Position)> {
    let (path, row) = s.trim_end_matches(':').rsplit_once(':')?;
    let row: usize = row.parse().ok()?;
    let path = path.into();
    let pos = Position::new(row.saturating_sub(1), 0);
    Some((path, pos))
}
