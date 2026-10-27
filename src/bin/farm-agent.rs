#[tokio::main]
async fn main() {
    if let Err(error) = farm::remote::agent::serve_stdio().await {
        eprintln!("farm-agent: {error}");
        std::process::exit(1);
    }
    // A finished shell can leave tokio's blocking stdin read waiting for input that never comes.
    std::process::exit(0);
}
