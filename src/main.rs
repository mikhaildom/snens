#[tokio::main]
async fn main() -> R<()> {
    production::entry().await
}

include!("cli.rs");
