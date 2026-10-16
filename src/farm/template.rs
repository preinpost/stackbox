use std::fmt;
use std::str::FromStr;

pub const BASE_IMAGE: &str = "farm-golden:rocky10";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Deployer,
    Lb,
    Infra,
    Keystone,
    Glance,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Deployer => "deployer",
            Role::Lb => "lb",
            Role::Infra => "infra",
            Role::Keystone => "keystone",
            Role::Glance => "glance",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Role {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "deployer" => Ok(Role::Deployer),
            "lb" => Ok(Role::Lb),
            "infra" => Ok(Role::Infra),
            "keystone" => Ok(Role::Keystone),
            "glance" => Ok(Role::Glance),
            other => Err(format!("unknown role '{other}'")),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct NodeSpec {
    pub role: Role,
    pub cpus: u8,
    pub memory_mib: u32,
    pub disk_mib: u32,
    /// Size of each ext4 disk backing Docker and containerd; `None` for nodes without containers.
    pub container_disk_mib: Option<u32>,
}

pub const DEFAULT_TEMPLATE: &[NodeSpec] = &[
    NodeSpec {
        role: Role::Deployer,
        cpus: 2,
        memory_mib: 2048,
        disk_mib: 20480,
        container_disk_mib: None,
    },
    NodeSpec {
        role: Role::Lb,
        cpus: 1,
        memory_mib: 1024,
        disk_mib: 10240,
        container_disk_mib: Some(10240),
    },
    NodeSpec {
        role: Role::Infra,
        cpus: 2,
        memory_mib: 4096,
        disk_mib: 10240,
        container_disk_mib: Some(20480),
    },
    NodeSpec {
        role: Role::Keystone,
        cpus: 2,
        memory_mib: 2048,
        disk_mib: 10240,
        container_disk_mib: Some(20480),
    },
    NodeSpec {
        role: Role::Glance,
        cpus: 2,
        memory_mib: 2048,
        disk_mib: 10240,
        container_disk_mib: Some(20480),
    },
];
