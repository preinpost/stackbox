mod farm;
mod kolla;
mod shell;

use anyhow::Result;
use clap::{Parser, Subcommand};

use farm::{DEFAULT_TEMPLATE, FarmNode};
use kolla::Phase;

#[derive(Parser)]
#[command(about = "Provision OpenStack farms on microsandbox VMs")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create the VMs of a farm from the default template
    Create { id: String },
    /// Deploy OpenStack onto a farm's VMs with kolla-ansible
    Deploy {
        id: String,
        /// Resume from this phase
        #[arg(long, value_enum, default_value_t = Phase::Prepare)]
        from: Phase,
    },
    /// Show the nodes of a farm
    Status { id: String },
    /// List farms
    List,
    /// Delete a farm and all of its nodes
    Delete { id: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Create { id } => print_nodes(&farm::create_farm(&id, DEFAULT_TEMPLATE).await?),
        Command::Deploy { id, from } => kolla::deploy(&id, from).await?,
        Command::Status { id } => print_nodes(&farm::get_farm(&id).await?),
        Command::List => {
            println!("{:<20} NODES", "FARM");
            for (id, nodes) in farm::list_farms().await? {
                println!("{id:<20} {nodes}");
            }
        }
        Command::Delete { id } => {
            let removed = farm::delete_farm(&id).await?;
            println!("removed {removed} node(s) from '{id}'");
        }
    }
    Ok(())
}

fn print_nodes(nodes: &[FarmNode]) {
    println!("{:<24} {:<10} {:<10} IP", "NAME", "ROLE", "STATUS");
    for node in nodes {
        let role = node.role.map_or("-", |r| r.as_str());
        let ip = node.ip.as_deref().unwrap_or("-");
        println!("{:<24} {:<10} {:<10} {}", node.name, role, node.status, ip);
    }
}
