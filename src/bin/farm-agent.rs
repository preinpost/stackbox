#[tokio::main]
async fn main() {
    // `farm-agent connect <coordinator URL>` keeps this host dialled in to a coordinator,
    // with the host token in FARM_AGENT_TOKEN. Otherwise serve one request on stdin.
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `agent switch <dir>` and `agent link [--setup] <conf>`, as `farm` takes them: the
    // host's link switch, and a guest's link to it. Neither boots a VM, so a bad
    // FARM_LIBKRUNFW_PATH must not keep them from starting.
    let link = match &args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["agent", "switch", dir] => Some(farm::link::host::serve_dir(dir.as_ref()).await),
        ["agent", "link", "--setup", conf] => Some(farm::link::guest::setup(conf.as_ref()).await),
        ["agent", "link", conf] => Some(farm::link::guest::run(conf.as_ref()).await),
        _ => None,
    };
    if let Some(result) = link {
        if let Err(error) = result {
            eprintln!("farm-agent: {error:#}");
            std::process::exit(1);
        }
        return;
    }
    // Everything below boots this host's farms, so a custom kernel must be registered before
    // the SDK resolves its runtime. Only failures here are fatal to these paths.
    if let Err(error) = farm::kernel::configure_sdk() {
        eprintln!("farm-agent: {error:#}");
        std::process::exit(1);
    }
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
