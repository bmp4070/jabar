//! Entry point for the `jabar` language server.

use lsp_server::Connection;

fn main() -> anyhow::Result<()> {
    jabar_server::init_tracing();
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
