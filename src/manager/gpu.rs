//! GPU load control from the coordinator.
//!
//! The coordinator computes a load's sample table, runs an OpenStack CLI script on the
//! control VM to write it into the instance's Nova metadata, and keeps a copy in the stackbox
//! record. The guest script reads the metadata; the portal only talks to this module.
//! Nothing here needs a real GPU, and nothing computes on the values.

use super::{NotFound, StackboxManager, bad_request, conflict, now, stackbox_components};
use crate::{
    gpu,
    model::{GpuConfig, GpuLoad, GpuLoadView, MAX_GPU_COUNT, StackboxInstance, StackboxStatus},
};
use anyhow::Result;

impl StackboxManager {
    /// A stackbox whose plan has a virtual GPU and Nova, whether or not it is deployed. The
    /// guest-script downloads only need this much.
    fn gpu_stackbox_config(&self, id: &str) -> Result<(crate::model::StackboxRecord, GpuConfig)> {
        let stackbox = self.get(id)?;
        if stackbox.status == StackboxStatus::Deleted {
            return Err(conflict("this stackbox was deleted"));
        }
        let gpu = stackbox
            .plan
            .as_ref()
            .and_then(|plan| plan.gpu.clone())
            .ok_or_else(|| {
                conflict(
                    "this stackbox has no virtual GPU; its gpu option is set at creation and cannot be added later (docs/gpu.md)",
                )
            })?;
        if !stackbox_components(&stackbox).iter().any(|c| c == "nova") {
            return Err(conflict(
                "GPU load control needs Nova in the stackbox's components",
            ));
        }
        Ok((stackbox, gpu))
    }

    /// A deployed GPU stackbox: its control VM answers the OpenStack CLI and its instances
    /// exist. The portal shows the guest-script downloads before this point.
    fn gpu_stackbox(&self, id: &str) -> Result<(crate::model::StackboxRecord, GpuConfig)> {
        let (stackbox, gpu) = self.gpu_stackbox_config(id)?;
        if stackbox.status != StackboxStatus::Ready || stackbox.active_job.is_some() {
            return Err(conflict(
                "GPU load control needs a deployed stackbox whose services are ready",
            ));
        }
        Ok((stackbox, gpu))
    }

    /// The raw guest script, for installing it by hand or in user-data.
    pub fn gpu_guest_script(&self, id: &str) -> Result<&'static str> {
        self.gpu_stackbox_config(id)?;
        Ok(gpu::GUEST_SCRIPT)
    }

    /// A user-data installer that writes the guest script and runs it once.
    pub fn gpu_user_data(&self, id: &str) -> Result<String> {
        self.gpu_stackbox_config(id)?;
        Ok(gpu::user_data_installer())
    }

    /// Every Nova server in the stackbox, with its flavor's GPU resource classes and the load
    /// stackbox applied. The list comes from the control VM's OpenStack CLI, so instances
    /// created outside the portal appear too.
    pub async fn instances(&self, id: &str) -> Result<Vec<StackboxInstance>> {
        let (stackbox, gpu) = self.gpu_stackbox(id)?;
        let backend = self.backend_for(&stackbox)?;
        let output = backend
            .control_script(id.to_owned(), gpu::INSTANCES_SCRIPT.to_owned())
            .await?;
        gpu::instances(&output, gpu.resource_class(), &stackbox.gpu_loads)
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
        let (stackbox, instance) = self.instance(id, server).await?;
        let count = instance.gpu_count.clamp(1, MAX_GPU_COUNT);
        let properties = gpu::metadata_properties(&instance.id, &load, count)?;
        let script = gpu::apply_script(&instance.id, &properties);
        self.backend_for(&stackbox)?
            .control_script(id.to_owned(), script)
            .await?;
        let server_id = instance.id.clone();
        self.update(|state| {
            let stackbox = state
                .stackboxes
                .get_mut(id)
                .ok_or_else(|| NotFound(format!("stackbox '{id}'")))?;
            stackbox.gpu_loads.insert(server_id.clone(), load.clone());
            stackbox.updated_at = now();
            Ok(())
        })?;
        Ok(GpuLoadView {
            server: instance.id,
            load,
            applied: true,
            properties: properties.keys().cloned().collect(),
        })
    }

    /// Remove stackbox's `stackbox_gpu_*` metadata and the coordinator's copy. The guest falls
    /// back to its default (one virtual GPU at 0%) at the next metadata refresh.
    pub async fn clear_gpu_load(&self, id: &str, server: &str) -> Result<GpuLoadView> {
        let (stackbox, instance) = self.instance(id, server).await?;
        let mut keys = instance.properties.clone();
        // If the control VM could not report the metadata (or an older agent omitted it),
        // the coordinator's copy still says which keys it wrote.
        if keys.is_empty() && stackbox.gpu_loads.contains_key(&instance.id) {
            let count = instance.gpu_count.clamp(1, MAX_GPU_COUNT);
            keys.push(gpu::COUNT_KEY.to_owned());
            for index in 0..count {
                keys.push(format!("{}{index}", gpu::LOAD_KEY_PREFIX));
            }
        }
        if !keys.is_empty() {
            let script = gpu::clear_script(&instance.id, &keys);
            self.backend_for(&stackbox)?
                .control_script(id.to_owned(), script)
                .await?;
        }
        let server_id = instance.id.clone();
        self.update(|state| {
            if let Some(stackbox) = state.stackboxes.get_mut(id) {
                stackbox.gpu_loads.remove(&server_id);
                stackbox.updated_at = now();
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
    ) -> Result<(crate::model::StackboxRecord, StackboxInstance)> {
        let (stackbox, _) = self.gpu_stackbox(id)?;
        let instances = self.instances(id).await?;
        let mut matches = instances
            .into_iter()
            .filter(|instance| instance.id == server || instance.name == server);
        let first = matches
            .next()
            .ok_or_else(|| NotFound(format!("instance '{server}' of stackbox '{id}'")))?;
        if matches.next().is_some() {
            return Err(bad_request(format!(
                "instance name '{server}' is ambiguous; use the server UUID"
            )));
        }
        Ok((stackbox, first))
    }
}
