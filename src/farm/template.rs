use std::fmt;
use std::str::FromStr;

pub const BASE_IMAGE: &str = "farm-golden:rocky10";

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    AllInOne,
    // Legacy roles remain readable so existing multi-VM farms can be inspected and deleted.
    Deployer,
    Lb,
    Infra,
    Keystone,
    Glance,
    /// Optional API services such as Placement and Horizon
    Control,
    /// Optional hypervisor and network agents
    Compute,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::AllInOne => "all-in-one",
            Role::Deployer => "deployer",
            Role::Lb => "lb",
            Role::Infra => "infra",
            Role::Keystone => "keystone",
            Role::Glance => "glance",
            Role::Control => "control",
            Role::Compute => "compute",
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
            "all-in-one" => Ok(Role::AllInOne),
            "deployer" => Ok(Role::Deployer),
            "lb" => Ok(Role::Lb),
            "infra" => Ok(Role::Infra),
            "keystone" => Ok(Role::Keystone),
            "glance" => Ok(Role::Glance),
            "control" => Ok(Role::Control),
            "compute" => Ok(Role::Compute),
            other => Err(format!("unknown role '{other}'")),
        }
    }
}

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct NodeSpec {
    pub role: Role,
    pub cpus: u8,
    pub memory_mib: u32,
    pub disk_mib: u32,
    /// Size of each ext4 disk backing Docker and containerd; `None` for nodes without containers.
    pub container_disk_mib: Option<u32>,
}

/// Base size of the farm's single all-in-one guest; selected services add their footprint.
pub const ALL_IN_ONE: NodeSpec = NodeSpec {
    role: Role::AllInOne,
    cpus: 2,
    memory_mib: 3072,
    disk_mib: 20480,
    container_disk_mib: Some(32768),
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_guest_is_all_in_one_and_legacy_roles_still_parse() {
        assert_eq!(ALL_IN_ONE.role, Role::AllInOne);
        assert_eq!(ALL_IN_ONE.cpus, 2);
        assert_eq!(ALL_IN_ONE.memory_mib, 3072);
        assert_eq!(ALL_IN_ONE.disk_mib, 20480);
        assert_eq!(ALL_IN_ONE.container_disk_mib, Some(32768));
        for role in [
            Role::AllInOne,
            Role::Deployer,
            Role::Lb,
            Role::Infra,
            Role::Keystone,
            Role::Glance,
            Role::Control,
            Role::Compute,
        ] {
            assert_eq!(role.as_str().parse::<Role>().unwrap(), role);
            assert_eq!(
                serde_json::from_str::<Role>(&serde_json::to_string(&role).unwrap()).unwrap(),
                role
            );
        }
    }
}
