//! Heimdall — PTY session supervisor.
//!
//! Owns the pty, manages process lifecycle, exposes a Unix socket for IPC.
//! Everything else is a client.

mod attach;
mod broadcast;
mod classify;
mod cli;
mod commands;
mod config;
mod pidfile;
mod protocol;
mod pty;
mod socket;
mod supervisor;
mod terminal;
mod util;

use clap::Parser;

fn main() -> anyhow::Result<()> {
    // Parse CLI before any Tokio runtime — fork() must happen single-threaded.
    let args = cli::Cli::parse();
    let cfg = config::load(args.config.as_deref())?;

    match args.command {
        cli::Command::Run {
            id,
            workdir,
            socket_dir,
            cols,
            rows,
            detach,
            log_file,
            log_level,
            log_filter,
            scrollback_bytes,
            classifier,
            idle_threshold_ms,
            debounce_ms,
            kill_process_group,
            session_env_var,
            cmd,
        } => {
            let params = cli::merge_run_args(
                cfg,
                cli::RunArgs {
                    id,
                    workdir,
                    socket_dir,
                    cols,
                    rows,
                    log_file,
                    log_level,
                    log_filter,
                    scrollback_bytes,
                    classifier,
                    idle_threshold_ms,
                    debounce_ms,
                    kill_process_group,
                    session_env_var,
                    cmd,
                },
            )?;
            if detach {
                supervisor::supervise(params)
            } else {
                attach::launch_and_attach(params)
            }
        }
        cli::Command::Attach { id, socket_dir } => {
            let dir = socket_dir.unwrap_or_else(|| cfg.socket_dir.clone());
            attach::attach(id, dir, &cfg)
        }
        cli::Command::Status { id, socket_dir } => {
            let dir = socket_dir.unwrap_or_else(|| cfg.socket_dir.clone());
            commands::status(id, dir, &cfg)
        }
        cli::Command::List { socket_dir } => {
            let dir = socket_dir.unwrap_or_else(|| cfg.socket_dir.clone());
            commands::list(dir)
        }
        cli::Command::Kill { id, socket_dir } => {
            let dir = socket_dir.unwrap_or_else(|| cfg.socket_dir.clone());
            commands::kill(id, dir)
        }
        cli::Command::Clean {
            socket_dir,
            older_than,
            force,
        } => {
            let dir = socket_dir.unwrap_or_else(|| cfg.socket_dir.clone());
            commands::clean(dir, &older_than, !force)
        }
    }
}
