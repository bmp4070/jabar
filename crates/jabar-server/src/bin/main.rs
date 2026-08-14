//! Entry point for the `jabar` language server.

fn main() {
    jabar_server::init_tracing();
    tracing::info!("jabar starting");
    eprintln!("jabar: the language server is not implemented yet");
    std::process::exit(1);
}
