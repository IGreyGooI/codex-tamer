#[tokio::main]
async fn main() {
    std::process::exit(codex_tamer::run().await);
}
