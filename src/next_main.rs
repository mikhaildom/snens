include!("cli.rs");
mod next;

#[tokio::main]
async fn main() -> R<()> {
    next::entry().await
}
