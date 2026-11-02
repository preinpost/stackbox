#[tokio::main]
async fn main() {
    // `farm-agent connect <coordinator URL>` keeps this host dialled in to a coordinator,
    // with the host token in FARM_AGENT_TOKEN. Otherwise serve one request on stdin.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let [mode, coordinator] = &args[..]
        && mode == "connect"
    {
        let Some(token) = std::env::var("FARM_AGENT_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
        else {
            eprintln!("farm-agent: set FARM_AGENT_TOKEN to the host token");
            std::process::exit(2);
        };
        let agent = std::sync::Arc::new(farm::remote::agent::Agent::local());
        if let Err(error) = farm::remote::dial::run(coordinator, &token, agent).await {
            eprintln!("farm-agent: {error:#}");
            std::process::exit(1);
        }
        return;
    }
    // The same arguments as `farm agent worker`, so either binary starts its deploy
    // workers alike.
    if let [agent, worker, job] = &args[..]
        && agent == "agent"
        && worker == "worker"
    {
        if let Err(error) = farm::remote::agent::serve_worker(job.as_ref()).await {
            eprintln!("farm-agent worker: {error:#}");
            std::process::exit(1);
        }
        std::process::exit(0);
    }
    if let Err(error) = farm::remote::agent::serve_stdio().await {
        eprintln!("farm-agent: {error}");
        std::process::exit(1);
    }
    // A finished shell can leave tokio's blocking stdin read waiting for input that never comes.
    std::process::exit(0);
}
