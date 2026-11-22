//! GPU load control from the coordinator.
//!
//! The coordinator computes a load's sample table, runs an OpenStack CLI script on the
//! control VM to write it into the instance's Nova metadata, and keeps a copy in the farm
//! record. The guest script reads the metadata; the portal only talks to this module.
//! Nothing here needs a real GPU, and nothing computes on the values.

use super::{FarmManager, NotFound, bad_request, conflict, farm_components, now};
use crate::{
    gpu,
    model::{FarmInstance, FarmStatus, GpuConfig, GpuLoad, GpuLoadView, MAX_GPU_COUNT},
};
use anyhow::Result;

impl FarmManager {
    /// A farm whose plan has a virtual GPU and Nova, whether or not it is deployed. The
    /// guest-script downloads only need this much.
    fn gpu_farm_config(&self, id: &str) -> Result<(crate::model::FarmRecord, GpuConfig)> {
        let farm = self.get(id)?;
        if farm.status == FarmStatus::Deleted {
            return Err(conflict("this farm was deleted"));
        }
        let gpu = farm
            .plan
            .as_ref()
            .and_then(|plan| plan.gpu.clone())
            .ok_or_else(|| {
                conflict(
                    "this farm has no virtual GPU; its gpu option is set at creation and cannot be added later (docs/gpu.md)",
                )
            })?;
        if !farm_components(&farm).iter().any(|c| c == "nova") {
            return Err(conflict(
                "GPU load control needs Nova in the farm's components",
            ));
        }
        Ok((farm, gpu))
    }

    /// A deployed GPU farm: its control VM answers the OpenStack CLI and its instances
    /// exist. The portal shows the guest-script downloads before this point.
    fn gpu_farm(&self, id: &str) -> Result<(crate::model::FarmRecord, GpuConfig)> {
        let (farm, gpu) = self.gpu_farm_config(id)?;
        if farm.status != FarmStatus::Ready || farm.active_job.is_some() {
            return Err(conflict(
                "GPU load control needs a deployed farm whose services are ready",
            ));
        }
        Ok((farm, gpu))
    }

    /// The raw guest script, for installing it by hand or in user-data.
    pub fn gpu_guest_script(&self, id: &str) -> Result<&'static str> {
        self.gpu_farm_config(id)?;
        Ok(gpu::GUEST_SCRIPT)
    }

    /// A user-data installer that writes the guest script and runs it once.
    pub fn gpu_user_data(&self, id: &str) -> Result<String> {
        self.gpu_farm_config(id)?;
        Ok(gpu::user_data_installer())
    }

    /// Every Nova server in the farm, with its flavor's GPU resource classes and the load
    /// farm applied. The list comes from the control VM's OpenStack CLI, so instances
    /// created outside the portal appear too.
    pub async fn instances(&self, id: &str) -> Result<Vec<FarmInstance>> {
        let (farm, gpu) = self.gpu_farm(id)?;
        let backend = self.backend_for(&farm)?;
        let output = backend
            .control_script(id.to_owned(), gpu::INSTANCES_SCRIPT.to_owned())
            .await?;
        gpu::instances(&output, gpu.resource_class(), &farm.gpu_loads)
    }

    /// The load for one server, by UUID or unique name. A server that never got a load
    /// shows the default with `applied: false`.
    pub async fn gpu_load(&self, id: &str, server: &str) -> Result<GpuLoadView> {
        let (_, instance) = self.instance(id, server).await?;
        Ok(GpuLoadView {
            server: instance.id,
            load: instance.load.unwrap_or_default(),
            applied: instance.applied,
            properties: instance.properties,
        })
    }

    /// Compute `load`'s samples and write them into the instance's metadata. The guest
    /// picks the sample for the current second; Nova's metadata cache makes the change
    /// visible within about 15 seconds.
    pub async fn set_gpu_load(&self, id: &str, server: &str, load: GpuLoad) -> Result<GpuLoadView> {
        load.validate()
            .map_err(|error| bad_request(error.to_string()))?;
        let (farm, instance) = self.instance(id, server).await?;
        let count = instance.gpu_count.clamp(1, MAX_GPU_COUNT);
        let properties = gpu::metadata_properties(&instance.id, &load, count)?;
        let script = gpu::apply_script(&instance.id, &properties);
        self.backend_for(&farm)?
            .control_script(id.to_owned(), script)
            .await?;
        let server_id = instance.id.clone();
        self.update(|state| {
            let farm = state
                .farms
                .get_mut(id)
                .ok_or_else(|| NotFound(format!("farm '{id}'")))?;
            farm.gpu_loads.insert(server_id.clone(), load.clone());
            farm.updated_at = now();
            Ok(())
        })?;
        Ok(GpuLoadView {
            server: instance.id,
            load,
            applied: true,
            properties: properties.keys().cloned().collect(),
        })
    }

    /// Remove farm's `farm_gpu_*` metadata and the coordinator's copy. The guest falls
    /// back to its default (one virtual GPU at 0%) at the next metadata refresh.
    pub async fn clear_gpu_load(&self, id: &str, server: &str) -> Result<GpuLoadView> {
        let (farm, instance) = self.instance(id, server).await?;
        let mut keys = instance.properties.clone();
        // If the control VM could not report the metadata (or an older agent omitted it),
        // the coordinator's copy still says which keys it wrote.
        if keys.is_empty() && farm.gpu_loads.contains_key(&instance.id) {
            let count = instance.gpu_count.clamp(1, MAX_GPU_COUNT);
            keys.push(gpu::COUNT_KEY.to_owned());
            for index in 0..count {
                keys.push(format!("{}{index}", gpu::LOAD_KEY_PREFIX));
            }
        }
        if !keys.is_empty() {
            let script = gpu::clear_script(&instance.id, &keys);
            self.backend_for(&farm)?
                .control_script(id.to_owned(), script)
                .await?;
        }
        let server_id = instance.id.clone();
        self.update(|state| {
            if let Some(farm) = state.farms.get_mut(id) {
                farm.gpu_loads.remove(&server_id);
                farm.updated_at = now();
            }
            Ok(())
        })?;
        Ok(GpuLoadView {
            server: instance.id,
            load: GpuLoad::default(),
            applied: false,
            properties: Vec::new(),
        })
    }

    async fn instance(
        &self,
        id: &str,
        server: &str,
    ) -> Result<(crate::model::FarmRecord, FarmInstance)> {
        let (farm, _) = self.gpu_farm(id)?;
        let instances = self.instances(id).await?;
        let mut matches = instances
            .into_iter()
            .filter(|instance| instance.id == server || instance.name == server);
        let first = matches
            .next()
            .ok_or_else(|| NotFound(format!("instance '{server}' of farm '{id}'")))?;
        if matches.next().is_some() {
            return Err(bad_request(format!(
                "instance name '{server}' is ambiguous; use the server UUID"
            )));
        }
        Ok((farm, first))
    }
}
