//! Wiring only: parse the arguments, point diagnostics at stderr, dispatch, and turn a failure
//! into a non-zero exit with one clear line.

use std::io::IsTerminal;
use std::process::ExitCode;

use clap::Parser;
use session_search::cli;
use tracing_subscriber::EnvFilter;

fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    init_tracing(cli.verbose, cli.no_color);

    match cli::run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        // `session-search search foo | head` closes stdout under us. That is the pipeline
        // working as intended, not an error worth a message or a failing status.
        Err(err) if is_broken_pipe(&err) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("session-search: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Diagnostics go to **stderr**, always: stdout carries the results (and, once the `mcp`
/// subcommand lands, the stdio transport owns it outright). `$RUST_LOG` wins over `-v`.
fn init_tracing(verbose: u8, no_color: bool) {
    let level = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter = EnvFilter::try_from_default_env()
        // Third-party crates (tantivy is chatty at info) stay at warn unless `$RUST_LOG`
        // asks otherwise: `-v` is about this program's own progress.
        .unwrap_or_else(|_| EnvFilter::new(format!("warn,session_search={level}")));
    let ansi = !no_color
        && std::io::stderr().is_terminal()
        && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(ansi)
        .with_target(false)
        .init();
}

fn is_broken_pipe(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
    })
}
