use std::fmt;
use std::str::FromStr;

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    AllInOne,
    // Legacy roles remain readable so existing multi-VM stackboxes can be inspected and deleted.
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
    /// Position among the stackbox's compute nodes, from 1; only multi-node compute nodes have one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<u8>,
    pub cpus: u8,
    pub memory_mib: u32,
    pub disk_mib: u32,
    /// Size of each ext4 disk backing Docker and containerd; `None` for nodes without containers.
    pub container_disk_mib: Option<u32>,
}

/// Base size of the stackbox's single all-in-one guest; selected services add their footprint.
pub const ALL_IN_ONE: NodeSpec = NodeSpec {
    role: Role::AllInOne,
    index: None,
    cpus: 2,
    memory_mib: 3072,
    disk_mib: 20480,
    container_disk_mib: Some(32768),
};
/// Base size of a multi-node stackbox's control VM, which also runs the network node and
/// Kolla; the control-plane share of each selected service adds to it.
pub const CONTROL: NodeSpec = NodeSpec {
    role: Role::Control,
    cpus: 4,
    ..ALL_IN_ONE
};
/// A multi-node stackbox's compute VM: nova-compute, libvirt and the Open vSwitch agent use
/// about 0.8 GiB; the rest is room for a few small instances.
pub const COMPUTE: NodeSpec = NodeSpec {
    role: Role::Compute,
    ..ALL_IN_ONE
};
/// Compute nodes a multi-node stackbox can have.
pub const MAX_COMPUTES: u8 = 3;

/// The VM name after `<stackbox>-`, such as `control` or `compute-2`; the all-in-one VM has none.
pub fn suffix(role: Role, index: Option<u8>) -> Option<String> {
    match (role, index) {
        (Role::AllInOne, _) => None,
        (role, Some(index)) => Some(format!("{role}-{index}")),
        (role, None) => Some(role.to_string()),
    }
}
/// Guest hostname. Every stackbox uses the same ones, so a snapshot keeps its identity elsewhere.
pub fn hostname(role: Role, index: Option<u8>) -> String {
    match suffix(role, index) {
        None => "stackbox-aio".into(),
        Some(suffix) => format!("stackbox-{suffix}"),
    }
}
/// Fixed service address on the stackbox's management network (`stackbox0`).
pub fn address(role: Role, index: Option<u8>) -> String {
    match (role, index) {
        (Role::Compute, Some(index)) => format!("10.254.0.{}", 10 + u16::from(index)),
        (Role::Control, _) => "10.254.0.10".into(),
        _ => "10.254.0.2".into(),
    }
}
impl NodeSpec {
    pub fn hostname(&self) -> String {
        hostname(self.role, self.index)
    }
    pub fn address(&self) -> String {
        address(self.role, self.index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nodes_have_fixed_names_hostnames_and_addresses() {
        let compute = |index| NodeSpec {
            index: Some(index),
            ..COMPUTE
        };
        assert_eq!(suffix(Role::AllInOne, None), None);
        assert_eq!(suffix(Role::Control, None).as_deref(), Some("control"));
        assert_eq!(suffix(Role::Compute, Some(2)).as_deref(), Some("compute-2"));
        // Legacy multi-VM nodes keep their `<stackbox>-<role>` names.
        assert_eq!(suffix(Role::Lb, None).as_deref(), Some("lb"));
        assert_eq!(ALL_IN_ONE.hostname(), "stackbox-aio");
        assert_eq!(ALL_IN_ONE.address(), "10.254.0.2");
        assert_eq!(CONTROL.hostname(), "stackbox-control");
        assert_eq!(CONTROL.address(), "10.254.0.10");
        assert_eq!(compute(1).hostname(), "stackbox-compute-1");
        assert_eq!(compute(MAX_COMPUTES).address(), "10.254.0.13");
        // Old plans have no index and still read.
        let old: NodeSpec = serde_json::from_str(
            r#"{"role":"all-in-one","cpus":2,"memory_mib":3072,"disk_mib":20480,"container_disk_mib":32768}"#,
        )
        .unwrap();
        assert_eq!(old.index, None);
        assert!(!serde_json::to_string(&old).unwrap().contains("index"));
    }

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
