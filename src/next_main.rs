include!("cli.rs");
mod next;
mod surface_client;
mod surface_runtime;
mod surface_server;
mod surface_tui;

#[tokio::main]
async fn main() -> R<()> {
    next::entry().await
}
