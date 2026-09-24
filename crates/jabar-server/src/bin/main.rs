//! Entry point for the `jabar` language server.

use lsp_server::Connection;

fn main() -> anyhow::Result<()> {
    if std::env::args_os().nth(1).is_some_and(|arg| arg == "--version" || arg == "-V") {
        println!("jabar {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    jabar_server::init_tracing();
    // Before anything else, so `t_ns` and startup durations count from process
    // entry. A no-op unless `JABAR_BENCH_LOG` is set.
    jabar_server::bench::init();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "jabar starting");

    // stdio is the transport every client uses; stdout therefore carries the
    // protocol and nothing else. Logs go to stderr, set up above.
    let (connection, io_threads) = Connection::stdio();
    let result = jabar_server::run_server(connection);

    // Joined even on failure, so the process does not exit while the writer
    // thread still holds an unflushed response.
    io_threads.join()?;
    result?;

    tracing::info!("jabar stopped");
    Ok(())
}
