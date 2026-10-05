include!("cli.rs");

#[tokio::main]
async fn main() -> R<()> {
    legacy_main().await
}
