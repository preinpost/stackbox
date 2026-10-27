use super::*;
use crate::remote::{
    HostInspection, HostJob, HostJobEvent, HostJobKind, HostStatus, RegisterHost, RemoteHost,
    SealedCredentials, StoredHost, ssh::Session,
};

impl FarmManager {
    pub fn hosts(&self) -> Result<Vec<RemoteHost>> {
        let store = self
            .0
            .store
            .lock()
            .map_err(|_| anyhow!("state mutex poisoned"))?;
        Ok(store
            .state
            .hosts
            .values()
            .map(|s| {
                let mut host = s.host.clone();
                host.capacity = host
                    .inspection
                    .as_ref()
                    .map(|i| crate::remote::HostCapacity {
                        memory_budget_mib: i.memory_total_mib * 80 / 100,
                        memory_reserved_mib: store
                            .state
                            .farms
                            .values()
                            .filter(|f| {
                                f.host_id.as_deref() == Some(&host.id)
                                    && f.status != FarmStatus::Deleted
                            })
                            .map(FarmRecord::memory_mib)
                            .sum(),
                    });
                host
            })
            .collect())
    }
    fn stored_host(&self, id: &str) -> Result<StoredHost> {
        self.0
            .store
            .lock()
            .map_err(|_| anyhow!("state mutex poisoned"))?
            .state
            .hosts
            .get(id)
            .cloned()
            .ok_or_else(|| NotFound(format!("host '{id}'")).into())
    }
    pub fn host(&self, id: &str) -> Result<RemoteHost> {
        self.hosts()?
            .into_iter()
            .find(|h| h.id == id)
            .ok_or_else(|| NotFound(format!("host '{id}'")).into())
    }
    pub fn register_host(&self, request: RegisterHost) -> Result<RemoteHost> {
        request.validate()?;
        request.credentials.validate()?;
        let mut stored = StoredHost {
            credential_ref: uuid::Uuid::new_v4().to_string(),
            credentials: SealedCredentials {
                nonce: String::new(),
                ciphertext: String::new(),
            },
            host: RemoteHost {
                id: request.id,
                address: request.address.to_ascii_lowercase(),
                port: request.port,
                username: request.username,
                fingerprint: request.fingerprint,
                auth_method: request.credentials.method(),
                status: HostStatus::Registered,
                inspection: None,
                agent: None,
                capacity: None,
                active_job: None,
                error: None,
                created_at: now(),
                updated_at: now(),
            },
        };
        stored.credentials = self.0.vault.seal(&request.credentials, &stored.aad())?;
        self.update(|state| {
            if !self.is_accepting() {
                return Err(conflict("daemon is draining"));
            }
            if state.hosts.contains_key(&stored.host.id) {
                return Err(conflict("host id already exists"));
            }
            if state
                .hosts
                .values()
                .any(|h| h.host.address == stored.host.address && h.host.port == stored.host.port)
            {
                return Err(conflict("this SSH address and port are already registered"));
            }
            let result = stored.host.clone();
            state.hosts.insert(result.id.clone(), stored);
            Ok(result)
        })
    }
    pub fn unregister_host(&self, id: &str) -> Result<()> {
        self.update(|state| {
            if !self.is_accepting() {
                return Err(conflict("daemon is draining"));
            }
            let host = state
                .hosts
                .get(id)
                .ok_or_else(|| NotFound(format!("host '{id}'")))?;
            if host.host.active_job.is_some() {
                return Err(conflict("host has an active job"));
            }
            if state
                .farms
                .values()
                .any(|f| f.host_id.as_deref() == Some(id) && f.status != FarmStatus::Deleted)
            {
                return Err(conflict(
                    "delete all farms on this host before unregistering it",
                ));
            }
            // This removes only coordinator records and encrypted credentials, never remote VMs/files.
            state.hosts.remove(id);
            state.host_jobs.retain(|_, job| job.host_id != id);
            Ok(())
        })
    }
    pub fn update_host_credentials(&self, request: RegisterHost) -> Result<RemoteHost> {
        request.validate()?;
        request.credentials.validate()?;
        self.update(|state| {
            self.ensure_accepting()?;
            if state
                .farms
                .values()
                .any(|f| f.host_id.as_deref() == Some(&request.id) && f.active_job.is_some())
            {
                return Err(conflict("a farm operation is running on this host"));
            }
            let stored = state
                .hosts
                .get_mut(&request.id)
                .ok_or_else(|| NotFound(format!("host '{}'", request.id)))?;
            if stored.host.active_job.is_some() {
                return Err(conflict("host has an active job"));
            }
            if stored.host.address != request.address.to_ascii_lowercase()
                || stored.host.port != request.port
                || stored.host.username != request.username
            {
                return Err(conflict(
                    "credential updates must keep the SSH address, port and username unchanged",
                ));
            }
            stored.host.fingerprint = request.fingerprint;
            stored.host.auth_method = request.credentials.method();
            stored.credential_ref = uuid::Uuid::new_v4().to_string();
            stored.credentials = self.0.vault.seal(&request.credentials, &stored.aad())?;
            stored.host.status = HostStatus::Registered;
            stored.host.error = None;
            stored.host.updated_at = now();
            if let Some(inspection) = &mut stored.host.inspection {
                inspection.vm_boot_verified = false;
            }
            Ok(stored.host.clone())
        })
    }
    pub fn host_jobs(&self) -> Result<Vec<HostJob>> {
        Ok(self
            .0
            .store
            .lock()
            .map_err(|_| anyhow!("state mutex poisoned"))?
            .state
            .host_jobs
            .values()
            .cloned()
            .collect())
    }
    pub fn host_job(&self, id: &str) -> Result<HostJob> {
        self.0
            .store
            .lock()
            .map_err(|_| anyhow!("state mutex poisoned"))?
            .state
            .host_jobs
            .get(id)
            .cloned()
            .ok_or_else(|| NotFound(format!("host job '{id}'")).into())
    }
    pub fn start_host_job(&self, id: &str, kind: HostJobKind) -> Result<HostJob> {
        let job = self.update(|state| {
            if !self.is_accepting() {
                return Err(conflict("daemon is draining"));
            }
            if state
                .farms
                .values()
                .any(|f| f.host_id.as_deref() == Some(id) && f.active_job.is_some())
            {
                return Err(conflict("a farm operation is running on this host"));
            }
            let host = &mut state
                .hosts
                .get_mut(id)
                .ok_or_else(|| NotFound(format!("host '{id}'")))?
                .host;
            if host.active_job.is_some() {
                return Err(conflict("host already has an active job"));
            }
            let job = HostJob {
                id: uuid::Uuid::new_v4().to_string(),
                host_id: id.into(),
                kind,
                status: JobStatus::Queued,
                events: vec![],
                error: None,
                created_at: now(),
                finished_at: None,
            };
            host.active_job = Some(job.id.clone());
            host.error = None;
            host.status = match kind {
                HostJobKind::Check => HostStatus::Checking,
                HostJobKind::Prepare | HostJobKind::InstallAgent => HostStatus::Preparing,
            };
            host.updated_at = now();
            state.host_jobs.insert(job.id.clone(), job.clone());
            Ok(job)
        })?;
        let manager = self.clone();
        let job_id = job.id.clone();
        tokio::spawn(async move {
            let result = manager.run_host_job(&job_id).await;
            if let Err(error) = manager.finish_host_job(&job_id, result) {
                eprintln!("failed to persist host job result: {error:#}");
            }
        });
        Ok(job)
    }
    fn host_event(&self, job: &str, message: &str) -> Result<()> {
        self.update(|state| {
            let job = state
                .host_jobs
                .get_mut(job)
                .ok_or_else(|| NotFound("host job".into()))?;
            job.status = JobStatus::Running;
            job.events.push(HostJobEvent {
                at: now(),
                message: message.into(),
            });
            Ok(())
        })
    }
    fn host_inspection(&self, id: &str, inspection: HostInspection) -> Result<()> {
        self.update(|state| {
            let host = &mut state
                .hosts
                .get_mut(id)
                .ok_or_else(|| NotFound("host".into()))?
                .host;
            host.inspection = Some(inspection);
            host.updated_at = now();
            Ok(())
        })
    }
    async fn run_host_job(&self, id: &str) -> Result<HostInspection> {
        let _permit = self.0.host_gate.acquire().await?;
        let job = self.host_job(id)?;
        let stored = self.stored_host(&job.host_id)?;
        self.host_event(id, "Verifying pinned SSH host key and authenticating")?;
        let credentials = self.0.vault.unseal(&stored.credentials, &stored.aad())?;
        let ssh = Session::connect(&stored.host, credentials).await?;
        self.host_event(
            id,
            "Inspecting OS, architecture, KVM, memory, disk, msb and Docker",
        )?;
        let mut inspection = ssh.inspect().await?;
        // A read-only check may retain the previous successful boot evidence for the same runtime.
        inspection.vm_boot_verified = inspection.runtime_ready()
            && inspection.golden_image_available
            && stored
                .host
                .inspection
                .as_ref()
                .is_some_and(|i| i.vm_boot_verified && i.msb_version == inspection.msb_version);
        if matches!(job.kind, HostJobKind::Prepare) {
            inspection.vm_boot_verified = false;
        }
        self.host_inspection(&job.host_id, inspection.clone())?;
        if matches!(job.kind, HostJobKind::InstallAgent) {
            anyhow::ensure!(
                inspection.compatible,
                "agent installation requires Linux x86_64 or aarch64"
            );
            self.install_host_agent(&ssh, &job).await?;
            return Ok(inspection);
        }
        if matches!(job.kind, HostJobKind::Check) {
            if let Some(agent) = &stored.host.agent {
                self.host_event(id, "Checking the installed Farm execution agent")?;
                let coordinator_id = self
                    .0
                    .store
                    .lock()
                    .map_err(|_| anyhow!("state mutex poisoned"))?
                    .state
                    .coordinator_id
                    .clone();
                let result = ssh
                    .rpc(
                        &agent.artifact.sha256,
                        &crate::remote::protocol::Request {
                            version: crate::remote::protocol::VERSION,
                            coordinator_id,
                            operation: crate::remote::protocol::Operation::Hello,
                        },
                        None,
                    )
                    .await;
                if !matches!(result, Ok(crate::remote::protocol::Reply::Hello { version: crate::remote::protocol::VERSION, ref arch, supports_single_vm: true, ref build_id }) if arch == &agent.artifact.arch && build_id == crate::BUILD_ID)
                {
                    self.update(|state| {
                        state
                            .hosts
                            .get_mut(&job.host_id)
                            .context("host missing")?
                            .host
                            .agent = None;
                        Ok(())
                    })?;
                    anyhow::bail!(
                        "the installed Farm execution agent was built from a different revision than this coordinator ({build}); run `task agent-build` and install the agent again",
                        build = crate::BUILD_ID
                    );
                }
            }
            return Ok(inspection);
        }
        anyhow::ensure!(
            inspection.compatible,
            "preparation requires Linux x86_64 or aarch64"
        );
        anyhow::ensure!(
            inspection.kvm_available,
            "/dev/kvm is not accessible; enable nested virtualization / KVM permissions on the target first"
        );
        anyhow::ensure!(
            inspection.memory_available_mib >= 768,
            "at least 768 MiB available memory is required for the verification VM"
        );
        anyhow::ensure!(
            inspection.disk_available_mib >= 8192,
            "at least 8 GiB free space is required for runtime and image preparation"
        );
        if let Some(version) = &inspection.msb_version {
            anyhow::ensure!(
                version == "0.7.8",
                "an incompatible msb is already installed; migrate it to 0.7.8 explicitly before preparing this host"
            );
        }
        self.host_event(
            id,
            "Installing or verifying msb 0.7.8 and libkrunfw (release checksum verified)",
        )?;
        ssh.install().await?;
        inspection = ssh.inspect().await?;
        self.host_inspection(&job.host_id, inspection.clone())?;
        if !inspection.golden_image_available {
            anyhow::ensure!(
                inspection.docker_available,
                "Docker is unavailable to the SSH user; configure Docker access before building the golden image"
            );
            self.host_event(
                id,
                "Building and loading farm-golden:rocky10; this can take up to 20 minutes",
            )?;
            ssh.golden_image().await?;
        } else {
            self.host_event(
                id,
                "Existing farm-golden:rocky10 found; keeping the cached image",
            )?;
        }
        self.host_event(
            id,
            "Booting a 512 MiB verification VM, executing a command and removing it",
        )?;
        ssh.smoke(id).await?;
        inspection = ssh.inspect().await?;
        anyhow::ensure!(
            inspection.runtime_ready() && inspection.golden_image_available,
            "host prerequisites changed during preparation; inspect the host and retry"
        );
        inspection.vm_boot_verified = true;
        self.host_event(id, "VM boot, command execution and cleanup verified")?;
        if self.0.artifacts.contains_key(&inspection.arch) {
            self.install_host_agent(&ssh, &job).await?;
        } else {
            self.host_event(
                id,
                &format!(
                    "No {} farm-agent is loaded; preparation finished without installing the execution agent",
                    inspection.arch
                ),
            )?;
        }
        Ok(inspection)
    }
    async fn install_host_agent(&self, ssh: &Session, job: &HostJob) -> Result<()> {
        let arch = self
            .stored_host(&job.host_id)?
            .host
            .inspection
            .as_ref()
            .context("host has no inspection")?
            .arch
            .clone();
        let artifact = self.artifact_for(&arch)?;
        // A stale artifact would install an agent that cannot parse this daemon's requests.
        artifact_built_by_daemon(&artifact)?;
        let coordinator = self
            .0
            .store
            .lock()
            .map_err(|_| anyhow!("state mutex poisoned"))?
            .state
            .coordinator_id
            .clone();
        self.host_event(
            &job.id,
            "Uploading farm-agent, verifying its SHA256 and checking the protocol",
        )?;
        let agent = ssh.install_agent(&artifact, &coordinator).await?;
        self.update(|state| {
            state
                .hosts
                .get_mut(&job.host_id)
                .context("host missing")?
                .host
                .agent = Some(agent);
            Ok(())
        })
    }
    fn finish_host_job(&self, id: &str, result: Result<HostInspection>) -> Result<()> {
        self.update(|state| {
            let job = state
                .host_jobs
                .get_mut(id)
                .ok_or_else(|| NotFound("host job".into()))?;
            let host = &mut state
                .hosts
                .get_mut(&job.host_id)
                .ok_or_else(|| NotFound("host".into()))?
                .host;
            match result {
                Ok(inspection) => {
                    host.status = inspection.status();
                    host.inspection = Some(inspection);
                    host.error = None;
                    job.status = JobStatus::Succeeded;
                }
                Err(error) => {
                    // Transport errors are sanitized by Session; never persist remote command output.
                    let message = error.to_string();
                    host.status = HostStatus::Failed;
                    host.error = Some(message.clone());
                    job.error = Some(message);
                    job.status = JobStatus::Failed;
                }
            }
            job.finished_at = Some(now());
            host.active_job = None;
            host.updated_at = now();
            Ok(())
        })
    }
}

/// Rejects an artifact that was built from other sources than the running coordinator.
pub(crate) fn artifact_built_by_daemon(artifact: &crate::remote::artifact::Artifact) -> Result<()> {
    check_artifact_build(&artifact.info.arch, &artifact.info.build_id)
}

fn check_artifact_build(arch: &str, build_id: &str) -> Result<()> {
    if build_id == crate::BUILD_ID {
        return Ok(());
    }
    Err(anyhow::anyhow!(
        "the {arch} farm-agent artifact was built from {build_id} but this coordinator is {daemon}; rebuild it with `scripts/build-agent.sh {arch}` and install the agent again",
        daemon = crate::BUILD_ID
    ))
}

/// Test hook: the same check without a real artifact file.
#[cfg(test)]
pub(crate) fn artifact_built_by_daemon_for_test(arch: &str, build_id: &str) -> Result<()> {
    check_artifact_build(arch, build_id)
}
