use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use farm::{
    api,
    kolla::Phase,
    manager::{FarmManager, ManagerOptions, default_state_dir},
    model::{self, CreateRequest, DeployRequest, Job, JobStatus, PlanRequest},
};
use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
};

#[derive(Parser)]
#[command(about = "Manage OpenStack farms on microsandbox VMs")]
struct Cli {
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    /// Total guest-memory reservation budget; defaults to 80% of host RAM
    #[arg(long, global = true)]
    memory_budget_mib: Option<u64>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Run the REST API daemon (FARM_API_TOKEN enables bearer authentication)
    Serve {
        #[arg(long, default_value = "127.0.0.1:8080")]
        bind: SocketAddr,
        /// Address on which newly created farms publish OpenStack services
        #[arg(long, default_value = "127.0.0.1")]
        public_bind: IpAddr,
    },
    Catalog {
        #[arg(long)]
        json: bool,
    },
    Plan {
        #[arg(long, value_delimiter = ',')]
        components: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    Create {
        id: String,
        /// Registered remote host; omit to execute locally
        #[arg(long)]
        host_id: Option<String>,
        #[arg(long, value_delimiter = ',')]
        components: Vec<String>,
        #[arg(long, default_value = "localhost")]
        external_host: String,
        #[arg(long)]
        owner: Option<String>,
        /// UTC Unix seconds; metadata only
        #[arg(long)]
        expires_at: Option<u64>,
    },
    Deploy {
        id: String,
        #[arg(long, value_enum, default_value_t=Phase::Prepare)]
        from: Phase,
    },
    Status {
        id: String,
        #[arg(long)]
        json: bool,
    },
    List {
        #[arg(long)]
        json: bool,
    },
    Delete {
        id: String,
    },
    Endpoints {
        id: String,
        #[arg(long)]
        json: bool,
    },
    Logs {
        id: String,
        #[arg(long, default_value_t = 0)]
        after: u64,
    },
    /// Print the OpenAPI document without opening the state store
    Openapi,
}
fn print_json(value: &impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
async fn wait(manager: &FarmManager, job: Job) -> Result<()> {
    println!("job {}: {:?}", job.id, job.kind);
    let job = manager.wait(&job.id).await?;
    if job.status != JobStatus::Succeeded {
        bail!(
            "{}",
            job.error.unwrap_or_else(|| format!("job {:?}", job.status))
        );
    }
    Ok(())
}
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // Planning and schema commands need neither msb nor the daemon's writer lock.
    match &cli.command {
        Command::Catalog { json } => {
            let c = model::catalog();
            if *json {
                return print_json(&c);
            }
            for component in c.components {
                let status = if component.required {
                    "required"
                } else if component.available {
                    "optional"
                } else {
                    "not deployable yet"
                };
                let depends = if component.depends_on.is_empty() {
                    String::new()
                } else {
                    format!(", needs {}", component.depends_on.join(", "))
                };
                println!("{} ({status}{depends})", component.id);
            }
            for constraint in c.constraints {
                println!("{constraint}");
            }
            return Ok(());
        }
        Command::Plan { components, json } => {
            let p = model::plan(&PlanRequest {
                components: components.clone(),
            })?;
            if *json {
                return print_json(&p);
            }
            println!(
                "{} nodes, {} MiB guest memory, {} MiB provisioned disk",
                p.nodes.len(),
                p.memory_mib,
                p.disk_mib
            );
            for node in p.nodes {
                println!(
                    "{:<12} {} MiB, {} vCPUs",
                    node.role, node.memory_mib, node.cpus
                );
            }
            for warning in p.warnings {
                println!("{warning}");
            }
            return Ok(());
        }
        Command::Openapi => return print_json(&api::openapi_document()),
        _ => {}
    }
    let mut options = ManagerOptions::local(cli.state_dir.unwrap_or_else(default_state_dir));
    if let Some(limit) = cli.memory_budget_mib {
        options.memory_budget_mib = limit;
    }
    let token = std::env::var("FARM_API_TOKEN").ok();
    if let Command::Serve { bind, public_bind } = &cli.command {
        anyhow::ensure!(
            bind.ip().is_loopback() || token.is_some(),
            "non-loopback API binding requires FARM_API_TOKEN"
        );
        anyhow::ensure!(
            token.as_ref().is_none_or(|t| !t.trim().is_empty()),
            "FARM_API_TOKEN cannot be empty"
        );
        options.public_bind = *public_bind;
    }
    let manager = FarmManager::open(options).await?;
    match cli.command {
        Command::Serve { bind, .. } => api::serve(manager, bind, token).await?,
        Command::Create {
            id,
            host_id,
            components,
            external_host,
            owner,
            expires_at,
        } => {
            let job = manager.create(CreateRequest {
                id: id.clone(),
                host_id,
                components,
                external_host,
                owner,
                expires_at,
            })?;
            wait(&manager, job).await?;
            print_json(&manager.get(&id)?)?;
        }
        Command::Deploy { id, from } => {
            wait(&manager, manager.deploy(&id, DeployRequest { from })?).await?;
        }
        Command::Delete { id } => {
            wait(&manager, manager.delete(&id)?).await?;
            println!("deleted farm '{id}'");
        }
        Command::Status { id, json } => {
            let farm = manager.get(&id)?;
            if json {
                print_json(&farm)?;
            } else {
                println!("{}: {:?}", farm.id, farm.status);
                if let Some(error) = farm.error {
                    println!("{error}");
                }
                println!("{:<24} {:<10} IP", "NAME", "STATUS");
                for node in farm.nodes {
                    println!(
                        "{:<24} {:<10} {}",
                        node.name,
                        node.status,
                        node.ip.as_deref().unwrap_or("-")
                    );
                }
            }
        }
        Command::List { json } => {
            let farms = manager.list()?;
            if json {
                print_json(&farms)?;
            } else {
                for farm in farms {
                    println!(
                        "{:<24} {:?} ({} nodes)",
                        farm.id,
                        farm.status,
                        farm.nodes.len()
                    );
                }
            }
        }
        Command::Endpoints { id, json: _ } => print_json(&manager.endpoints(&id).await?)?,
        Command::Logs { id, after } => {
            for entry in manager.logs(&id, after, 1000)? {
                println!("[{}] {}", entry.id, entry.message);
            }
        }
        Command::Catalog { .. } | Command::Plan { .. } | Command::Openapi => unreachable!(),
    }
    Ok(())
}
