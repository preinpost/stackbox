//! The virtual GPU guest script and the load farm writes into Nova instance metadata.
//!
//! farm does not emulate a GPU: it installs a small `nvidia-smi` that reads its values
//! from the instance's metadata. The portal writes a load (`GpuLoad`), farm samples its
//! pattern into at most [`MAX_SAMPLES`] utilization values, and the guest derives the
//! remaining fields (temperature, power, fan, clocks, pstate) with integer arithmetic.
//! The table fits in one Nova property value (255 characters), so a load travels over
//! the metadata service that every instance already has.
//!
//! The generated shell scripts are run on the control VM by the farm backend. Their
//! arguments are shell-quoted here; `metadata_properties` is also the value the
//! coordinator↔agent boundary test follows into the `openstack server set` command.

use crate::model::{FarmInstance, GpuLoad, GpuPattern, GpuProfile, MAX_GPU_COUNT};
use anyhow::{Result, bail, ensure};
use serde::Deserialize;
use std::collections::BTreeMap;

/// The guest script installed as `/usr/bin/nvidia-smi`.
pub const GUEST_SCRIPT: &str = include_str!("../guest/gpu/nvidia-smi.sh");
/// The user-data installer; `#__FARM_GPU_SCRIPT__` is replaced with [`GUEST_SCRIPT`].
const INSTALLER_TEMPLATE: &str = include_str!("../guest/gpu/user-data.sh");
/// A Nova instance property value is at most 255 characters.
pub const MAX_PROPERTY_VALUE: usize = 255;
/// Samples in one period. Twenty-four 0-100 values and the other fields fit in 255
/// characters with the GPU UUID.
pub const MAX_SAMPLES: u32 = 24;
/// Where the installer writes the fake tool.
pub const GUEST_BINARY: &str = "/usr/bin/nvidia-smi";
/// The key the portal and the guest use for the number of virtual GPUs.
pub const COUNT_KEY: &str = "farm_gpu_count";
/// The key prefix of one virtual GPU's load.
pub const LOAD_KEY_PREFIX: &str = "farm_gpu_";
/// The metadata version prefix the guest understands.
const VERSION: &str = "v1";

/// The installer a user can pass as user-data (CirrOS and other images that do not
/// apply Nova vendor data). It refuses to replace the tool when a real GPU is present.
pub fn user_data_installer() -> String {
    INSTALLER_TEMPLATE.replace("#__FARM_GPU_SCRIPT__", GUEST_SCRIPT.trim_end())
}

/// A cloud-config document for Nova's static vendor data: install the script on every
/// instance of a GPU farm. cloud-init merges it without user-data; images that are not
/// cloud-init (CirrOS) ignore it and use [`user_data_installer`] instead.
pub fn vendordata_cloud_config() -> String {
    let mut config = String::from(
        "#cloud-config\n\
         write_files:\n\
         \x20 - path: /usr/local/sbin/farm-gpu-install\n\
         \x20   permissions: '0755'\n\
         \x20   owner: root:root\n\
         \x20   content: |\n",
    );
    for line in user_data_installer().lines() {
        if line.is_empty() {
            config.push('\n');
        } else {
            config.push_str("      ");
            config.push_str(line);
            config.push('\n');
        }
    }
    config.push_str("runcmd:\n  - [sh, /usr/local/sbin/farm-gpu-install]\n");
    config
}

/// The one-line JSON Kolla writes to `/etc/kolla/config/nova/vendordata.json`.
pub fn vendordata_json() -> String {
    serde_json::json!({ "cloud-init": vendordata_cloud_config() }).to_string()
}

impl GpuLoad {
    /// Refuses values the guest cannot express or the portal cannot draw.
    pub fn validate(&self) -> Result<()> {
        if self.min > self.max {
            bail!("a GPU load's min must not be greater than its max");
        }
        if self.max > 100 {
            bail!("a GPU load's utilization is a percentage (0-100)");
        }
        if !(1..=3600).contains(&self.period_s) {
            bail!("a GPU load's period is 1-3600 seconds");
        }
        if self.memory_ratio > 100 {
            bail!("a GPU load's memory ratio is a percentage (0-100)");
        }
        Ok(())
    }

    /// The utilization samples of one period, 0-100. The count is the period in
    /// seconds capped at [`MAX_SAMPLES`], so a one-second period is one sample.
    pub fn samples(&self) -> Vec<u8> {
        let n = self.period_s.clamp(1, MAX_SAMPLES) as usize;
        (0..n).map(|i| self.sample(i, n)).collect()
    }

    fn sample(&self, index: usize, n: usize) -> u8 {
        let low = self.min.min(self.max) as f64;
        let high = self.max.max(self.min) as f64;
        let span = high - low;
        let mid = (low + high) / 2.0;
        let t = index as f64 / n as f64;
        let value = match self.pattern {
            GpuPattern::Constant => mid,
            GpuPattern::Sine => mid + span / 2.0 * (std::f64::consts::TAU * t).sin(),
            GpuPattern::Triangle => {
                if t < 0.5 {
                    low + span * (t * 2.0)
                } else {
                    high - span * ((t - 0.5) * 2.0)
                }
            }
            GpuPattern::Square => {
                if t < 0.5 {
                    high
                } else {
                    low
                }
            }
            GpuPattern::Sawtooth => low + span * t,
            GpuPattern::Random => {
                // Deterministic: applying the same load again draws the same table, and
                // the portal's preview can compute it without a random seed from farm.
                let mut x = (index as u64)
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(u64::from(self.period_s).wrapping_mul(1442695040888963407))
                    .wrapping_add((self.min as u64) << 32)
                    .wrapping_add(self.max as u64);
                x ^= x >> 33;
                x = x.wrapping_mul(0xff51afd7ed558ccd);
                x ^= x >> 33;
                low + span * (x % 1000) as f64 / 1000.0
            }
            GpuPattern::Spike => {
                if n == 1 || index == n / 2 {
                    high
                } else {
                    low
                }
            }
        };
        (value.round() as i64).clamp(0, 100) as u8
    }

    /// One virtual GPU's metadata value. `index` rotates the table so the cards of a
    /// multi-GPU instance do not move in lockstep; the UUID is deterministic from the
    /// instance and the card.
    pub fn metadata_value(&self, server_uuid: &str, index: u32, count: u32) -> String {
        let count = count.max(1);
        let mut samples = self.samples();
        if count > 1 && !samples.is_empty() {
            let shift = index as usize * samples.len() / count as usize;
            samples.rotate_left(shift);
        }
        let samples = samples
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "{VERSION};p:{};q:{};n:{};x:{};d:{};m:{};u:{samples};id:{}",
            self.profile.id(),
            self.pattern.id(),
            self.min,
            self.max,
            self.period_s,
            self.memory_ratio,
            gpu_uuid(server_uuid, index),
        )
    }

    /// Reads a value farm wrote. A value from another version, or one that no longer
    /// validates, is `None`; the caller then shows the default load.
    pub fn decode(value: &str) -> Option<Self> {
        let mut load = Self::default();
        let mut version = false;
        for part in value.split(';') {
            if part == VERSION {
                version = true;
                continue;
            }
            let (key, value) = part.split_once(':')?;
            match key {
                "p" => load.profile = GpuProfile::from_id(value)?,
                "q" => load.pattern = GpuPattern::from_id(value)?,
                "n" => load.min = value.parse().ok()?,
                "x" => load.max = value.parse().ok()?,
                "d" => load.period_s = value.parse().ok()?,
                "m" => load.memory_ratio = value.parse().ok()?,
                // The sample table is derived from the fields above; the UUID is not a
                // property of the load.
                "u" | "id" => {}
                _ => return None,
            }
        }
        if !version {
            return None;
        }
        load.validate().ok()?;
        Some(load)
    }
}

/// The deterministic UUID of a virtual GPU: UUIDv5 over the instance UUID and index,
/// so every guest of an instance reports the same `GPU-...` after a reboot.
pub fn gpu_uuid(server_uuid: &str, index: u32) -> String {
    let name = format!("farm-gpu:{server_uuid}:{index}");
    format!(
        "GPU-{}",
        uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, name.as_bytes())
    )
}

/// The Nova instance properties of `load` for an instance with `gpu_count` virtual
/// GPUs. Every value is checked against Nova's 255-character limit.
pub fn metadata_properties(
    server_uuid: &str,
    load: &GpuLoad,
    gpu_count: u32,
) -> Result<BTreeMap<String, String>> {
    load.validate()?;
    let count = gpu_count.clamp(1, MAX_GPU_COUNT);
    let mut properties = BTreeMap::new();
    properties.insert(COUNT_KEY.to_owned(), count.to_string());
    for index in 0..count {
        let value = load.metadata_value(server_uuid, index, count);
        ensure!(
            value.len() <= MAX_PROPERTY_VALUE,
            "a virtual GPU load is {} characters; Nova accepts {MAX_PROPERTY_VALUE}",
            value.len()
        );
        properties.insert(format!("{LOAD_KEY_PREFIX}{index}"), value);
    }
    Ok(properties)
}

/// The instance's load as the portal should show it: the coordinator's copy, or the
/// value still in the instance's metadata after the state was lost.
pub fn load_of(stored: Option<&GpuLoad>, properties: &BTreeMap<String, String>) -> Option<GpuLoad> {
    stored.cloned().or_else(|| {
        properties
            .get("farm_gpu_0")
            .and_then(|v| GpuLoad::decode(v))
    })
}

/// Whether the instance's metadata carries a load farm wrote.
pub fn applied(properties: &BTreeMap<String, String>) -> bool {
    properties
        .keys()
        .any(|key| key.starts_with(LOAD_KEY_PREFIX))
}

/// One entry of the control VM's instance list. `resources` is the flavor's
/// `resources:*` map, already stripped of the prefix.
#[derive(Deserialize)]
struct RawInstance {
    id: String,
    name: String,
    status: String,
    #[serde(default)]
    project: String,
    #[serde(default)]
    flavor: String,
    #[serde(default)]
    properties: BTreeMap<String, String>,
    #[serde(default)]
    resources: BTreeMap<String, String>,
}

/// Lists every server and its flavor's GPU resource classes through the control VM's
/// OpenStack CLI. `farm_gpu_*` metadata comes with each server, so the portal can tell
/// whether a load is applied without a second call.
pub const INSTANCES_SCRIPT: &str = r#"set -euo pipefail
. "${FARM_ADMIN_OPENRC:-/etc/kolla/admin-openrc.sh}"
python3 - <<'PY'
import json
import os
import subprocess

OS = os.environ.get('FARM_OPENSTACK_CLI', '/opt/osc/bin/openstack')


def run(*args):
    out = subprocess.check_output(
        [OS, *args, '-f', 'json'], text=True, stderr=subprocess.DEVNULL
    )
    return json.loads(out)


def flavor_name(reference):
    if isinstance(reference, dict):
        return reference.get('original_name') or reference.get('name') or reference.get('id') or ''
    return str(reference or '')


flavors = {}
instances = []
for server in run('server', 'list', '--all-projects'):
    server_id = server.get('ID') or server.get('id')
    try:
        show = run('server', 'show', server_id)
    except Exception:
        # The server may have been deleted between the list and the show.
        continue
    flavor = flavor_name(show.get('flavor') or server.get('Flavor') or server.get('flavor'))
    if flavor not in flavors:
        properties = {}
        try:
            detail = run('flavor', 'show', flavor)
            properties = detail.get('properties') or detail.get('Properties') or {}
        except Exception:
            pass
        flavors[flavor] = {
            str(key)[len('resources:'):]: str(value)
            for key, value in properties.items()
            if str(key).startswith('resources:')
        }
    properties = {str(key): str(value) for key, value in (show.get('properties') or {}).items()}
    instances.append({
        'id': show.get('id') or server_id,
        'name': show.get('name') or server.get('Name') or '',
        'status': show.get('status') or server.get('Status') or '',
        'project': str(show.get('project_id') or server.get('Project') or server.get('Project ID') or server.get('project_id') or ''),
        'flavor': flavor,
        'properties': properties,
        'resources': flavors[flavor],
    })
print(json.dumps(instances))
PY
"#;

/// Parses [`INSTANCES_SCRIPT`]'s JSON and adds the load the portal should show.
pub fn instances(
    output: &str,
    resource_class: &str,
    loads: &BTreeMap<String, GpuLoad>,
) -> Result<Vec<FarmInstance>> {
    let raw: Vec<RawInstance> = serde_json::from_str(output.trim())
        .map_err(|error| anyhow::anyhow!("cannot read the control VM's instance list: {error}"))?;
    Ok(raw
        .into_iter()
        .map(|instance| {
            let resources: BTreeMap<String, u32> = instance
                .resources
                .iter()
                .filter_map(|(class, count)| Some((class.clone(), count.parse().ok()?)))
                .collect();
            // A flavor may ask for a standard class while the farm advertises a custom
            // one; show whichever GPU class it names.
            let gpu_count = resources
                .get(resource_class)
                .or_else(|| resources.get("VGPU"))
                .or_else(|| resources.get("PGPU"))
                .copied()
                .unwrap_or(0);
            let load = load_of(loads.get(&instance.id), &instance.properties);
            let properties = instance
                .properties
                .keys()
                .filter(|key| key.starts_with(LOAD_KEY_PREFIX))
                .cloned()
                .collect();
            FarmInstance {
                id: instance.id,
                name: instance.name,
                status: instance.status,
                project: instance.project,
                flavor: instance.flavor,
                gpu_count,
                resources,
                load,
                applied: applied(&instance.properties),
                properties,
            }
        })
        .collect())
}

/// The control VM script that writes `properties` to `server`. The value is what the
/// guest reads; quoting keeps semicolons and commas inside the argument.
pub fn apply_script(server: &str, properties: &BTreeMap<String, String>) -> String {
    let mut script = String::from(
        "set -euo pipefail\n. \"${FARM_ADMIN_OPENRC:-/etc/kolla/admin-openrc.sh}\"\nos=${FARM_OPENSTACK_CLI:-/opt/osc/bin/openstack}\n",
    );
    if properties.is_empty() {
        script.push_str(&format!(
            "$os server show {} >/dev/null\n",
            sh_quote(server)
        ));
        return script;
    }
    script.push_str(&format!("$os server set {} \\\n", sh_quote(server)));
    let last = properties.len() - 1;
    for (index, (key, value)) in properties.iter().enumerate() {
        script.push_str(&format!(
            "  --property {}",
            sh_quote(&format!("{key}={value}"))
        ));
        script.push_str(if index == last { "\n" } else { " \\\n" });
    }
    script
}

/// The control VM script that removes `keys` from `server`'s metadata. An empty key
/// list is a no-op, so a DELETE is safe on an instance that never got a load.
pub fn clear_script(server: &str, keys: &[String]) -> String {
    let mut script = String::from(
        "set -euo pipefail\n. \"${FARM_ADMIN_OPENRC:-/etc/kolla/admin-openrc.sh}\"\nos=${FARM_OPENSTACK_CLI:-/opt/osc/bin/openstack}\n",
    );
    if keys.is_empty() {
        script.push_str("# no farm_gpu_* property is set\n");
        return script;
    }
    script.push_str(&format!("$os server unset {} \\\n", sh_quote(server)));
    let last = keys.len() - 1;
    for (index, key) in keys.iter().enumerate() {
        script.push_str(&format!("  --property {}", sh_quote(key)));
        script.push_str(if index == last { "\n" } else { " \\\n" });
    }
    script
}

/// A shell single-quoted word, with the POSIX `'\''` escape for a quote inside it.
pub fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(pattern: GpuPattern) -> GpuLoad {
        GpuLoad {
            profile: GpuProfile::Sim8g,
            pattern,
            min: 10,
            max: 90,
            period_s: 24,
            memory_ratio: 30,
        }
    }

    #[test]
    fn sample_tables_follow_their_patterns() {
        let constant = load(GpuPattern::Constant).samples();
        assert_eq!(constant.len(), 24);
        assert!(constant.iter().all(|v| *v == 50));

        let sine = load(GpuPattern::Sine).samples();
        assert_eq!(sine.len(), 24);
        assert!(sine.iter().all(|v| (10..=90).contains(v)));
        assert!(sine[6] > 80 && sine[18] < 20);

        let triangle = load(GpuPattern::Triangle).samples();
        assert_eq!(triangle[0], 10);
        assert!(triangle[12] >= 90);
        assert!(triangle[23] < 20);

        let square = load(GpuPattern::Square).samples();
        assert!(square[..12].iter().all(|v| *v == 90));
        assert!(square[12..].iter().all(|v| *v == 10));

        let sawtooth = load(GpuPattern::Sawtooth).samples();
        assert_eq!(sawtooth[0], 10);
        assert!(sawtooth.windows(2).all(|w| w[0] <= w[1]));
        assert!(sawtooth[23] < 90);

        let spike = load(GpuPattern::Spike).samples();
        assert_eq!(spike.iter().filter(|v| **v == 90).count(), 1);
        assert_eq!(spike[12], 90);

        let random = load(GpuPattern::Random).samples();
        assert_eq!(random, load(GpuPattern::Random).samples());
        assert!(random.iter().all(|v| (10..=90).contains(v)));
        assert!(random.windows(2).any(|w| w[0] != w[1]));

        // A period shorter than the sample cap keeps one sample per second.
        let short = GpuLoad {
            period_s: 5,
            ..load(GpuPattern::Triangle)
        };
        assert_eq!(short.samples().len(), 5);
        // A long period still fits the metadata value.
        let long = GpuLoad {
            period_s: 3600,
            ..load(GpuPattern::Sine)
        };
        assert_eq!(long.samples().len(), MAX_SAMPLES as usize);
    }

    #[test]
    fn the_portal_preview_matches_the_guest_table() {
        // The same arrays are asserted by web/src/components/gpu-load.test.tsx. The
        // portal's preview must draw the table the guest will report.
        let load = |pattern| GpuLoad {
            pattern,
            min: 10,
            max: 90,
            period_s: 24,
            ..Default::default()
        };
        assert_eq!(load(GpuPattern::Constant).samples(), vec![50; 24]);
        assert_eq!(
            load(GpuPattern::Sine).samples(),
            vec![
                50, 60, 70, 78, 85, 89, 90, 89, 85, 78, 70, 60, 50, 40, 30, 22, 15, 11, 10, 11, 15,
                22, 30, 40
            ]
        );
        assert_eq!(
            load(GpuPattern::Triangle).samples(),
            vec![
                10, 17, 23, 30, 37, 43, 50, 57, 63, 70, 77, 83, 90, 83, 77, 70, 63, 57, 50, 43, 37,
                30, 23, 17
            ]
        );
        assert_eq!(
            load(GpuPattern::Square).samples(),
            vec![
                90, 90, 90, 90, 90, 90, 90, 90, 90, 90, 90, 90, 10, 10, 10, 10, 10, 10, 10, 10, 10,
                10, 10, 10
            ]
        );
        assert_eq!(
            load(GpuPattern::Sawtooth).samples(),
            vec![
                10, 13, 17, 20, 23, 27, 30, 33, 37, 40, 43, 47, 50, 53, 57, 60, 63, 67, 70, 73, 77,
                80, 83, 87
            ]
        );
        assert_eq!(
            load(GpuPattern::Random).samples(),
            vec![
                83, 36, 61, 42, 64, 40, 17, 20, 50, 25, 38, 42, 49, 85, 15, 80, 86, 85, 11, 42, 16,
                53, 16, 46
            ]
        );
        assert_eq!(
            load(GpuPattern::Spike).samples(),
            vec![
                10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 90, 10, 10, 10, 10, 10, 10, 10, 10,
                10, 10, 10
            ]
        );
    }

    #[test]
    fn a_load_round_trips_through_its_metadata_value() {
        for profile in GpuProfile::ALL {
            for pattern in GpuPattern::ALL {
                let load = GpuLoad {
                    profile,
                    pattern,
                    min: 0,
                    max: 100,
                    period_s: 3600,
                    memory_ratio: 100,
                };
                let value = load.metadata_value("11111111-2222-3333-4444-555555555555", 0, 1);
                assert_eq!(GpuLoad::decode(&value), Some(load.clone()), "{value}");
            }
        }
        assert_eq!(GpuLoad::decode("not a farm load"), None);
        assert_eq!(GpuLoad::decode("v1;p:nope;q:sine"), None);
        assert_eq!(
            GpuLoad::decode("v1;p:sim-8g;q:sine;n:90;x:10"),
            None,
            "min above max is refused"
        );
    }

    #[test]
    fn every_metadata_value_fits_the_nova_property_limit() {
        for profile in GpuProfile::ALL {
            for pattern in GpuPattern::ALL {
                let load = GpuLoad {
                    profile,
                    pattern,
                    min: 0,
                    max: 100,
                    period_s: 3600,
                    memory_ratio: 100,
                };
                let properties =
                    metadata_properties("11111111-2222-3333-4444-555555555555", &load, 24).unwrap();
                assert_eq!(properties.len(), 25);
                assert_eq!(properties[COUNT_KEY], "24");
                for (key, value) in &properties {
                    assert!(key.len() <= MAX_PROPERTY_VALUE);
                    assert!(
                        value.len() <= MAX_PROPERTY_VALUE,
                        "{key} is {} characters",
                        value.len()
                    );
                }
            }
        }
    }

    #[test]
    fn gpu_uuids_are_deterministic_and_differ_per_card() {
        let server = "11111111-2222-3333-4444-555555555555";
        let first = gpu_uuid(server, 0);
        assert_eq!(first, gpu_uuid(server, 0));
        assert_ne!(first, gpu_uuid(server, 1));
        assert_ne!(first, gpu_uuid("22222222-2222-3333-4444-555555555555", 0));
        assert!(first.starts_with("GPU-"));
        // UUIDv5 layout: 8-4-4-4-12 hex digits.
        let hex = &first["GPU-".len()..];
        assert_eq!(hex.len(), 36);
        assert_eq!(hex.as_bytes()[14], b'5');
    }

    #[test]
    fn the_installer_embeds_the_guest_script_and_refuses_a_real_gpu() {
        let installer = user_data_installer();
        assert!(installer.starts_with("#!/bin/sh\n"));
        assert!(installer.contains(GUEST_SCRIPT.trim_end()));
        assert!(!installer.contains("#__FARM_GPU_SCRIPT__"));
        assert!(installer.contains("10de:"));
        assert!(installer.contains("cat >/usr/bin/nvidia-smi"));

        let vendordata = vendordata_json();
        let value: serde_json::Value = serde_json::from_str(&vendordata).unwrap();
        let cloud = value["cloud-init"].as_str().unwrap();
        assert!(cloud.starts_with("#cloud-config\n"));
        assert!(cloud.contains("path: /usr/local/sbin/farm-gpu-install"));
        assert!(cloud.contains("runcmd:\n  - [sh, /usr/local/sbin/farm-gpu-install]"));
        // The installer is one YAML literal block: every line is indented, and the
        // block scalar keeps the guest script's own shebang as content.
        assert!(cloud.contains("      #!/bin/sh\n"));
        assert!(cloud.contains("      FARM_GPU_SCRIPT_EOF\n"));
    }

    #[cfg(unix)]
    #[test]
    fn the_generated_scripts_are_valid_posix_shell() {
        let dir = tempfile::tempdir().unwrap();
        let load = GpuLoad::default();
        let properties =
            metadata_properties("11111111-2222-3333-4444-555555555555", &load, 2).unwrap();
        for (name, script) in [
            ("user-data.sh", user_data_installer()),
            ("apply.sh", apply_script("server-1", &properties)),
            (
                "clear.sh",
                clear_script("server-1", &properties.keys().cloned().collect::<Vec<_>>()),
            ),
            ("instances.sh", INSTANCES_SCRIPT.to_owned()),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, script).unwrap();
            let status = std::process::Command::new("sh")
                .arg("-n")
                .arg(&path)
                .status()
                .unwrap();
            assert!(status.success(), "{name} is not valid shell");
        }
    }

    #[test]
    fn the_apply_script_quotes_each_property_as_one_argument() {
        let load = GpuLoad::default();
        let properties =
            metadata_properties("11111111-2222-3333-4444-555555555555", &load, 2).unwrap();
        let script = apply_script("server-1", &properties);
        assert!(script.starts_with("set -euo pipefail\n"));
        assert!(script.contains("$os server set 'server-1'"));
        for (key, value) in &properties {
            assert!(script.contains(&format!("--property '{key}={value}'")));
        }
        // A quote in a server name cannot end the shell word.
        assert!(apply_script("a'b", &properties).contains("'a'\\''b'"));
        assert!(clear_script("server-1", &[]).contains("no farm_gpu_* property"));
        assert!(
            clear_script("server-1", &["farm_gpu_0".into()]).contains("--property 'farm_gpu_0'")
        );
    }

    #[test]
    fn the_instance_list_reads_the_farm_load_and_its_flavor() {
        let load = GpuLoad {
            pattern: GpuPattern::Square,
            min: 0,
            max: 100,
            period_s: 10,
            memory_ratio: 50,
            ..Default::default()
        };
        let value = load.metadata_value("server-1", 0, 1);
        let output = serde_json::json!([{
            "id": "server-1",
            "name": "demo",
            "status": "ACTIVE",
            "project": "admin",
            "flavor": "farm.vgpu",
            "properties": {"farm_gpu_count": "1", "farm_gpu_0": value},
            "resources": {"VGPU": "1", "CUSTOM_OTHER": "2"},
        }])
        .to_string();
        let loads = BTreeMap::new();
        let listed = instances(&output, "VGPU", &loads).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].gpu_count, 1);
        assert_eq!(listed[0].resources["VGPU"], 1);
        assert!(listed[0].applied);
        assert_eq!(listed[0].load, Some(load.clone()));

        // A flavor with no GPU class shows 0; the load editor still defaults to one
        // virtual GPU because that is what the guest script reports without metadata.
        let output = serde_json::json!([{
            "id": "server-2", "name": "plain", "status": "ACTIVE",
            "flavor": "m1.small", "properties": {}, "resources": {},
        }])
        .to_string();
        let listed = instances(&output, "VGPU", &loads).unwrap();
        assert_eq!(listed[0].gpu_count, 0);
        assert!(!listed[0].applied);
        assert_eq!(listed[0].load, None);
    }

    #[test]
    fn a_state_without_gpu_loads_still_reads() {
        // The field must default so an older state.json keeps loading.
        let farm: crate::model::FarmRecord = serde_json::from_value(serde_json::json!({
            "id": "old",
            "status": "ready",
            "plan": null,
            "exposure": null,
            "owner": null,
            "expires_at": null,
            "nodes": [],
            "phase": null,
            "error": null,
            "active_job": null,
            "created_at": 1,
            "updated_at": 1,
        }))
        .unwrap();
        assert!(farm.gpu_loads.is_empty());
    }
}
