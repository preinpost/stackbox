#[tokio::main]
async fn main() {
    if let Err(error) = farm::remote::agent::serve_stdio().await {
        eprintln!("farm-agent: {error}");
        std::process::exit(1);
    }
}
