include!("cli.rs");

#[tokio::main]
async fn main() -> R<()> {
    production::entry().await
}
