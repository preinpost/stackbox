use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use farm::{
    api,
    kolla::Phase,
    manager::{FarmManager, ManagerOptions, default_state_dir},
    model::{
        self, CreateRequest, DeployRequest, ExportRequest, ImportRequest, Job, JobStatus,
        PlanRequest, SnapshotRecord, SnapshotRequest, SnapshotStatus,
    },
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
        /// Start from a snapshot instead of an empty VM (local only; replaces --components)
        #[arg(long, conflicts_with_all = ["components", "host_id"])]
        snapshot: Option<String>,
    },
    Deploy {
        id: String,
        #[arg(long, value_enum, default_value_t=Phase::Prepare)]
        from: Phase,
    },
    /// Check that a deployed farm's services answer again after its VM started from disk
    Settle {
        id: String,
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
    /// Freeze farms into snapshots, and move snapshots between hosts as archives
    #[command(subcommand)]
    Snapshot(SnapshotCommand),
    /// Print the OpenAPI document without opening the state store
    Openapi,
}
#[derive(Subcommand)]
enum SnapshotCommand {
    /// Freeze a farm's VM; it stops for the capture and starts again
    Create {
        farm: String,
        id: String,
        #[arg(long)]
        note: Option<String>,
    },
    List {
        #[arg(long)]
        json: bool,
    },
    /// Write a self-contained .msb archive (default: <state dir>/exports/<id>.msb)
    Export {
        id: String,
        /// Archive file or existing directory
        #[arg(long, short)]
        output: Option<PathBuf>,
    },
    Import {
        archive: PathBuf,
        /// Defaults to the archive's file name
        #[arg(long)]
        id: Option<String>,
    },
    Delete {
        id: String,
    },
}
fn print_json(value: &impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
async fn wait_snapshot(manager: &FarmManager, id: &str) -> Result<SnapshotRecord> {
    let snapshot = manager.wait_snapshot(id).await?;
    if snapshot.status == SnapshotStatus::Failed {
        bail!(
            "{}",
            snapshot.error.unwrap_or_else(|| "snapshot failed".into())
        );
    }
    if let Some(error) = &snapshot.error {
        bail!("{error}");
    }
    Ok(snapshot)
}
fn absolute(path: PathBuf) -> Result<String> {
    Ok(std::path::absolute(path)?.display().to_string())
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
            snapshot,
        } => {
            let job = manager.create(CreateRequest {
                id: id.clone(),
                host_id,
                components,
                external_host,
                owner,
                expires_at,
                snapshot_id: snapshot,
            })?;
            wait(&manager, job).await?;
            print_json(&manager.get(&id)?)?;
        }
        Command::Deploy { id, from } => {
            wait(&manager, manager.deploy(&id, DeployRequest { from })?).await?;
        }
        Command::Settle { id } => {
            wait(&manager, manager.settle(&id)?).await?;
            println!("{id}: services answer again");
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
        Command::Snapshot(command) => match command {
            SnapshotCommand::Create { farm, id, note } => {
                wait(
                    &manager,
                    manager.freeze(
                        &farm,
                        SnapshotRequest {
                            id: id.clone(),
                            note,
                        },
                    )?,
                )
                .await?;
                print_json(&manager.snapshot(&id)?)?;
            }
            SnapshotCommand::List { json } => {
                let snapshots = manager.snapshots()?;
                if json {
                    print_json(&snapshots)?;
                } else {
                    for s in snapshots {
                        println!(
                            "{:<24} {:?} {:<8} from {} ({}{})",
                            s.id,
                            s.status,
                            s.arch,
                            s.source_farm.as_deref().unwrap_or("archive"),
                            s.plan.components.join(","),
                            if s.deployed { ", deployed" } else { "" }
                        );
                    }
                }
            }
            SnapshotCommand::Export { id, output } => {
                manager.export_snapshot(
                    &id,
                    ExportRequest {
                        path: output.map(absolute).transpose()?,
                    },
                )?;
                let snapshot = wait_snapshot(&manager, &id).await?;
                let archive = snapshot.archive.expect("export records its archive");
                println!("{} ({} bytes)", archive.path, archive.size_bytes);
            }
            SnapshotCommand::Import { archive, id } => {
                let record = manager.import_snapshot(ImportRequest {
                    path: absolute(archive)?,
                    id,
                })?;
                print_json(&wait_snapshot(&manager, &record.id).await?)?;
            }
            SnapshotCommand::Delete { id } => {
                manager.delete_snapshot(&id).await?;
                println!("deleted snapshot '{id}'");
            }
        },
        Command::Catalog { .. } | Command::Plan { .. } | Command::Openapi => unreachable!(),
    }
    Ok(())
}
