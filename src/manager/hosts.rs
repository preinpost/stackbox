use super::*;
use crate::remote::{
    AgentHostToken, HostInspection, HostJob, HostJobEvent, HostJobKind, HostKind, HostStatus,
    MSB_VERSION, RegisterAgentHost, RegisterHost, RegisterLocalHost, RemoteHost, StoredHost,
    link::AgentLink,
    msb_outdated,
    protocol::{Operation, Reply, Request, VERSION},
    ssh::Session,
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
                let local_budget = self.0.options.memory_budget_mib;
                host.capacity = host
                    .inspection
                    .as_ref()
                    .map(|i| crate::remote::HostCapacity {
                        memory_budget_mib: match host.kind {
                            HostKind::Local => local_budget,
                            HostKind::Ssh | HostKind::Agent => i.memory_total_mib * 80 / 100,
                        },
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
                host.connected = match host.kind {
                    HostKind::Local => true,
                    HostKind::Agent => self.0.hub.connected(&host.id),
                    HostKind::Ssh => false,
                };
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
            credentials: None,
            token_sha256: None,
            host: RemoteHost {
                id: request.id,
                kind: HostKind::Ssh,
                address: request.address.to_ascii_lowercase(),
                port: request.port,
                username: request.username,
                fingerprint: request.fingerprint,
                auth_method: Some(request.credentials.method()),
                status: HostStatus::Registered,
                inspection: None,
                agent: None,
                capacity: None,
                active_job: None,
                error: None,
                connected: false,
                created_at: now(),
                updated_at: now(),
            },
        };
        stored.credentials = Some(self.0.vault.seal(&request.credentials, &stored.aad())?);
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
    /// Register the machine this coordinator runs on. Its agent is this binary, so it needs
    /// no address, credential or installation; check and prepare it like any other host.
    pub fn register_local_host(&self, request: RegisterLocalHost) -> Result<RemoteHost> {
        crate::model::validate_id(&request.id).map_err(|e| bad_request(e.to_string()))?;
        let host = RemoteHost {
            id: request.id,
            kind: HostKind::Local,
            address: "localhost".into(),
            port: 0,
            username: std::env::var("USER").unwrap_or_default(),
            fingerprint: String::new(),
            auth_method: None,
            status: HostStatus::Registered,
            inspection: None,
            agent: None,
            capacity: None,
            active_job: None,
            error: None,
            connected: false,
            created_at: now(),
            updated_at: now(),
        };
        self.update(|state| {
            self.ensure_accepting()?;
            if state.hosts.contains_key(&host.id) {
                return Err(conflict("host id already exists"));
            }
            if state.hosts.values().any(|h| h.host.kind == HostKind::Local) {
                return Err(conflict("the local host is already registered"));
            }
            state.hosts.insert(
                host.id.clone(),
                StoredHost {
                    host: host.clone(),
                    credential_ref: uuid::Uuid::new_v4().to_string(),
                    credentials: None,
                    token_sha256: None,
                },
            );
            Ok(host)
        })
    }
    /// Register a host whose agent will dial in with the returned token
    /// (`farm agent connect`). No address or credential is stored.
    pub fn register_agent_host(&self, request: RegisterAgentHost) -> Result<AgentHostToken> {
        crate::model::validate_id(&request.id).map_err(|e| bad_request(e.to_string()))?;
        let mut secret = [0u8; 32];
        getrandom::getrandom(&mut secret).map_err(|e| anyhow!("cannot generate a token: {e}"))?;
        let token = format!(
            "farm_{}",
            base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, secret)
        );
        let host = RemoteHost {
            id: request.id,
            kind: HostKind::Agent,
            address: String::new(),
            port: 0,
            username: String::new(),
            fingerprint: String::new(),
            auth_method: None,
            status: HostStatus::Registered,
            inspection: None,
            agent: None,
            capacity: None,
            active_job: None,
            error: None,
            connected: false,
            created_at: now(),
            updated_at: now(),
        };
        self.update(|state| {
            self.ensure_accepting()?;
            if state.hosts.contains_key(&host.id) {
                return Err(conflict("host id already exists"));
            }
            state.hosts.insert(
                host.id.clone(),
                StoredHost {
                    host: host.clone(),
                    credential_ref: uuid::Uuid::new_v4().to_string(),
                    credentials: None,
                    token_sha256: Some(token_digest(&token)),
                },
            );
            Ok(())
        })?;
        Ok(AgentHostToken { host, token })
    }
    /// The dial-in host a token belongs to.
    pub fn authenticate_agent(&self, token: &str) -> Result<String> {
        use subtle::ConstantTimeEq;
        let digest = token_digest(token);
        self.0
            .store
            .lock()
            .map_err(|_| anyhow!("state mutex poisoned"))?
            .state
            .hosts
            .values()
            .find(|stored| {
                stored.host.kind == HostKind::Agent
                    && stored
                        .token_sha256
                        .as_ref()
                        .is_some_and(|known| bool::from(known.as_bytes().ct_eq(digest.as_bytes())))
            })
            .map(|stored| stored.host.id.clone())
            .ok_or_else(|| NotFound("agent token".into()).into())
    }
    /// Hold a dial-in host's socket open for requests until the returned handle drops.
    pub fn attach_agent(&self, host_id: &str) -> crate::remote::hub::Attached {
        self.0.hub.attach(host_id)
    }
    /// The host's agent: the installed artifact on a remote host, this binary locally,
    /// or the agent's own connection for a dial-in host.
    pub(super) fn host_link(&self, stored: &StoredHost) -> Result<Arc<dyn AgentLink>> {
        Ok(match stored.host.kind {
            HostKind::Local => self.0.local_link.clone(),
            HostKind::Agent => Arc::new(crate::remote::hub::HubLink {
                hub: self.0.hub.clone(),
                host: stored.host.id.clone(),
            }),
            HostKind::Ssh => Arc::new(crate::remote::link::SshLink {
                host: stored.host.clone(),
                credentials: self.0.vault.unseal(
                    stored
                        .credentials
                        .as_ref()
                        .context("remote host has no credentials")?,
                    &stored.aad(),
                )?,
                digest: stored
                    .host
                    .agent
                    .as_ref()
                    .context("host has no installed agent")?
                    .artifact
                    .sha256
                    .clone(),
            }),
        })
    }
    fn coordinator_id(&self) -> Result<String> {
        Ok(self
            .0
            .store
            .lock()
            .map_err(|_| anyhow!("state mutex poisoned"))?
            .state
            .coordinator_id
            .clone())
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
            if stored.host.kind != HostKind::Ssh {
                return Err(conflict("only SSH hosts have credentials"));
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
            stored.host.auth_method = Some(request.credentials.method());
            stored.credential_ref = uuid::Uuid::new_v4().to_string();
            stored.credentials = Some(self.0.vault.seal(&request.credentials, &stored.aad())?);
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
        self.start_job(id, kind, false)
    }
    /// Reinstalls the agent on every idle SSH host that runs another build's agent, when this
    /// build's artifact for the host is loaded. The daemon runs it at startup, so rebuilding
    /// the coordinator does not leave its hosts on an agent it refuses to use.
    pub fn upgrade_stale_agents(&self) -> Result<Vec<HostJob>> {
        let mut stale = Vec::new();
        {
            let store = self
                .0
                .store
                .lock()
                .map_err(|_| anyhow!("state mutex poisoned"))?;
            for stored in store.state.hosts.values() {
                let host = &stored.host;
                if host.kind != HostKind::Ssh
                    || host.active_job.is_some()
                    || host
                        .agent
                        .as_ref()
                        .is_none_or(|agent| agent.artifact.build_id == crate::BUILD_ID)
                {
                    continue;
                }
                let current = host
                    .inspection
                    .as_ref()
                    .is_some_and(|inspection| self.has_current_agent(&inspection.platform()));
                if current {
                    stale.push(host.id.clone());
                } else {
                    eprintln!(
                        "host {} runs another build's farm-agent and no current artifact is loaded for it; run `task agent-build` and restart",
                        host.id
                    );
                }
            }
        }
        let mut jobs = Vec::new();
        for id in stale {
            match self.start_host_job(&id, HostJobKind::InstallAgent) {
                Ok(job) => jobs.push(job),
                Err(error) => eprintln!("cannot reinstall the farm-agent on {id}: {error:#}"),
            }
        }
        Ok(jobs)
    }
    /// Purges the host; `delete_snapshots` also deletes the snapshots, which only the local
    /// host keeps.
    pub fn start_purge(&self, id: &str, delete_snapshots: bool) -> Result<HostJob> {
        self.start_job(id, HostJobKind::Purge, delete_snapshots)
    }
    fn start_job(&self, id: &str, kind: HostJobKind, delete_snapshots: bool) -> Result<HostJob> {
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
            if host.kind != HostKind::Ssh && matches!(kind, HostJobKind::InstallAgent) {
                return Err(conflict(
                    "only SSH hosts get an agent installed; this host runs its own",
                ));
            }
            if delete_snapshots && host.kind != HostKind::Local {
                return Err(bad_request("only the local host keeps snapshots"));
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
                delete_snapshots,
            };
            host.active_job = Some(job.id.clone());
            host.error = None;
            host.status = match kind {
                HostJobKind::Check => HostStatus::Checking,
                HostJobKind::Prepare | HostJobKind::InstallAgent | HostJobKind::Purge => {
                    HostStatus::Preparing
                }
            };
            host.updated_at = now();
            state.host_jobs.insert(job.id.clone(), job.clone());
            Ok(job)
        })?;
        let manager = self.clone();
        let job_id = job.id.clone();
        let purging = matches!(kind, HostJobKind::Purge);
        tokio::spawn(async move {
            let result = manager.run_host_job(&job_id).await;
            let recorded = if purging {
                // A successful purge has already removed the host it would update.
                match result {
                    Ok(_) => Ok(()),
                    Err(error) => manager.fail_purge_job(&job_id, &format!("{error:#}")),
                }
            } else {
                manager.finish_host_job(&job_id, result)
            };
            if let Err(error) = recorded {
                eprintln!("failed to persist host job result: {error:#}");
            }
        });
        Ok(job)
    }
    /// Removes everything this coordinator put on the host: its farms' VMs, any VM left in the
    /// agent's registry, and then the host record itself. Sandboxes the agent does not own stay.
    async fn purge_host(&self, job: &HostJob) -> Result<HostInspection> {
        let host_id = job.host_id.clone();
        let stored = self.stored_host(&host_id)?;
        let farms: Vec<String> = self
            .list()?
            .into_iter()
            .filter(|farm| farm.host_id.as_deref() == Some(host_id.as_str()))
            .map(|farm| farm.id)
            .collect();
        if !farms.is_empty() {
            self.host_event(
                &job.id,
                &format!("Deleting {} farm(s) and their VMs", farms.len()),
            )?;
        }
        for farm in &farms {
            // This job owns the host's active_job slot, so bypass that guard.
            let delete = self.delete_farm(farm, true, false)?;
            let outcome = self.wait(&delete.id).await?;
            if outcome.status != crate::model::JobStatus::Succeeded {
                anyhow::bail!(
                    "deleting farm {farm} failed: {}; the host was left registered",
                    outcome.error.unwrap_or_else(|| "unknown error".into())
                );
            }
        }
        if job.delete_snapshots {
            let snapshots = self.snapshots()?;
            if !snapshots.is_empty() {
                self.host_event(
                    &job.id,
                    &format!("Deleting {} snapshot(s)", snapshots.len()),
                )?;
            }
            for snapshot in &snapshots {
                self.delete_snapshot(&snapshot.id).await.map_err(|error| {
                    anyhow::anyhow!(
                        "deleting snapshot {} failed: {error:#}; the host was left registered",
                        snapshot.id
                    )
                })?;
            }
        }

        self.host_event(
            &job.id,
            "Removing VM reservations the agent still holds for this coordinator",
        )?;
        let link = self
            .host_link(&self.stored_host(&host_id)?)
            .context("host has no installed agent to clean up with")?;
        let reply = link
            .call(
                Request {
                    version: VERSION,
                    coordinator_id: self.coordinator_id()?,
                    operation: Operation::Purge,
                },
                None,
            )
            .await
            .map_err(|error| {
                anyhow::anyhow!(
                    "the host's agent could not run the cleanup ({error:#}); install the current agent from Hosts and remove the host again"
                )
            })?;
        let purged = match reply {
            Reply::Purged { farms } => farms,
            _ => anyhow::bail!("unexpected remote purge response"),
        };
        self.host_event(
            &job.id,
            &format!("Removed {purged} reserved VM(s); unregistering the host"),
        )?;
        let mut finished = self.host_job(&job.id)?;
        // This job is the host's active job, and unregistering refuses while one is set.
        let inspection = stored.host.inspection.clone();
        self.update(|state| {
            if let Some(host) = state.hosts.get_mut(&host_id) {
                host.host.active_job = None;
            }
            Ok(())
        })?;
        self.unregister_host(&host_id)?;
        // Unregistering drops this host's job history; keep the purge job so the
        // caller polling its Location still sees the outcome.
        finished.status = JobStatus::Succeeded;
        finished.finished_at = Some(now());
        finished.events.push(HostJobEvent {
            at: now(),
            message: "Host removed".into(),
        });
        self.update(|state| {
            state
                .host_jobs
                .insert(finished.id.clone(), finished.clone());
            Ok(())
        })?;
        inspection.ok_or_else(|| anyhow::anyhow!("host has no inspection to report"))
    }

    /// Tell how the host gets its golden image, which preparation leaves to the first farm.
    fn golden_event(&self, job: &str, inspection: &HostInspection) -> Result<()> {
        let golden = crate::farm::golden::current();
        self.host_event(
            job,
            &if inspection.golden_image_available {
                format!("Golden image {golden} is built")
            } else {
                format!(
                    "Golden image {golden} is not built yet; the first farm builds it from {} with msb",
                    crate::farm::golden::DEFAULT_OS.base_image
                )
            },
        )
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
    /// Re-inspect an SSH host whose last inspection is older than `max_age` seconds, as a
    /// farm created on it is about to start. Returns whether it inspected the host.
    pub(super) async fn refresh_ssh_inspection(&self, host_id: &str, max_age: u64) -> Result<bool> {
        let stored = self.stored_host(host_id)?;
        let Some(previous) = stored.host.inspection.as_ref() else {
            anyhow::bail!("host '{host_id}' has never been inspected; check it under Hosts");
        };
        if stored.host.kind != HostKind::Ssh || now().saturating_sub(previous.checked_at) <= max_age {
            return Ok(false);
        }
        let credentials = self.0.vault.unseal(
            stored
                .credentials
                .as_ref()
                .context("remote host has no credentials")?,
            &stored.aad(),
        )?;
        let ssh = Session::connect(&stored.host, credentials).await?;
        let mut inspection = ssh.inspect().await?;
        // Keep the boot evidence of preparation while the runtime it verified is unchanged.
        inspection.vm_boot_verified = inspection.runtime_ready()
            && previous.vm_boot_verified
            && previous.msb_version == inspection.msb_version;
        let ready = inspection.vm_boot_verified;
        self.host_inspection(host_id, inspection)?;
        anyhow::ensure!(
            ready,
            "host '{host_id}' is no longer ready (KVM, msb or architecture changed); check and prepare it under Hosts"
        );
        Ok(true)
    }
    async fn run_host_job(&self, id: &str) -> Result<HostInspection> {
        let _permit = self.0.host_gate.acquire().await?;
        let job = self.host_job(id)?;
        if matches!(job.kind, HostJobKind::Purge) {
            return self.purge_host(&job).await;
        }
        let stored = self.stored_host(&job.host_id)?;
        if stored.host.kind != HostKind::Ssh {
            return self.run_agent_host_job(&job, &stored).await;
        }
        self.host_event(id, "Verifying pinned SSH host key and authenticating")?;
        let credentials = self.0.vault.unseal(
            stored
                .credentials
                .as_ref()
                .context("remote host has no credentials")?,
            &stored.aad(),
        )?;
        let ssh = Session::connect(&stored.host, credentials).await?;
        self.host_event(
            id,
            "Inspecting OS, architecture, KVM, memory, disk, msb and Docker",
        )?;
        let mut inspection = ssh.inspect().await?;
        // A read-only check may retain the previous successful boot evidence for the same runtime.
        inspection.vm_boot_verified = inspection.runtime_ready()
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
        self.host_event(id, &msb_install_step(inspection.msb_version.as_deref())?)?;
        ssh.install().await?;
        inspection = ssh.inspect().await?;
        self.host_inspection(&job.host_id, inspection.clone())?;
        self.host_event(
            id,
            "Booting a 512 MiB verification VM, executing a command and removing it",
        )?;
        ssh.smoke(id).await?;
        inspection = ssh.inspect().await?;
        anyhow::ensure!(
            inspection.runtime_ready(),
            "host prerequisites changed during preparation; inspect the host and retry"
        );
        inspection.vm_boot_verified = true;
        self.host_event(id, "VM boot, command execution and cleanup verified")?;
        self.golden_event(id, &inspection)?;
        if self.has_current_agent(&inspection.platform()) {
            self.install_host_agent(&ssh, &job).await?;
        } else {
            self.host_event(
                id,
                &format!(
                    "No {} farm-agent of this build is loaded or published; preparation finished without installing the execution agent",
                    inspection.platform()
                ),
            )?;
        }
        Ok(inspection)
    }
    /// Check or prepare a host whose agent needs no installation: this machine, or a host
    /// that dials in. Its agent inspects the host, installs or upgrades msb (Linux and Apple
    /// Silicon macOS) and boots the test VM.
    async fn run_agent_host_job(
        &self,
        job: &HostJob,
        stored: &StoredHost,
    ) -> Result<HostInspection> {
        let id = job.id.as_str();
        let link = self.host_link(stored)?;
        let coordinator_id = self.coordinator_id()?;
        let call = |operation| {
            link.call(
                Request {
                    version: VERSION,
                    coordinator_id: coordinator_id.clone(),
                    operation,
                },
                None,
            )
        };
        self.host_event(id, "Checking the host's farm agent")?;
        let hello = call(Operation::Hello).await?;
        anyhow::ensure!(
            matches!(hello, Reply::Hello { version: VERSION, ref build_id, .. } if build_id == crate::BUILD_ID),
            "the host's farm agent was built from other sources than this coordinator ({}); update it",
            crate::BUILD_ID
        );
        self.host_event(
            id,
            "Inspecting the host: OS, architecture, hypervisor, memory, disk, msb and the golden image",
        )?;
        let inspect = || async {
            match call(Operation::Inspect).await? {
                Reply::Inspection { inspection } => Ok::<_, anyhow::Error>(inspection),
                _ => anyhow::bail!("unexpected farm-agent inspection response"),
            }
        };
        let mut inspection = inspect().await?;
        inspection.vm_boot_verified = !matches!(job.kind, HostJobKind::Prepare)
            && inspection.runtime_ready()
            && stored
                .host
                .inspection
                .as_ref()
                .is_some_and(|i| i.vm_boot_verified && i.msb_version == inspection.msb_version);
        self.host_inspection(&job.host_id, inspection.clone())?;
        if !matches!(job.kind, HostJobKind::Prepare) {
            return Ok(inspection);
        }
        anyhow::ensure!(
            inspection.compatible,
            "the host's OS or architecture is not supported"
        );
        anyhow::ensure!(
            inspection.kvm_available,
            "the host has no usable hypervisor (/dev/kvm on Linux)"
        );
        if !inspection.msb_compatible {
            self.host_event(id, &msb_install_step(inspection.msb_version.as_deref())?)?;
            call(Operation::InstallMsb).await?;
            inspection = inspect().await?;
            self.host_inspection(&job.host_id, inspection.clone())?;
            anyhow::ensure!(
                inspection.msb_compatible,
                "msb {MSB_VERSION} was installed, but the host still runs {}",
                inspection.msb_version.as_deref().unwrap_or("no msb")
            );
        }
        self.host_event(
            id,
            "Booting a 512 MiB verification VM, executing a command and removing it",
        )?;
        call(Operation::VerifyVm).await?;
        inspection = inspect().await?;
        anyhow::ensure!(
            inspection.runtime_ready(),
            "the host's prerequisites changed during preparation; check it and retry"
        );
        inspection.vm_boot_verified = true;
        self.host_event(id, "VM boot, command execution and cleanup verified")?;
        self.golden_event(id, &inspection)?;
        Ok(inspection)
    }
    async fn install_host_agent(&self, ssh: &Session, job: &HostJob) -> Result<()> {
        let platform = self
            .stored_host(&job.host_id)?
            .host
            .inspection
            .as_ref()
            .context("host has no inspection")?
            .platform();
        if self.loaded_current(&platform).is_none() && self.has_current_agent(&platform) {
            self.host_event(
                &job.id,
                &format!("Fetching the published {platform} farm-agent"),
            )?;
        }
        let artifact = self.artifact_for(&platform).await?;
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
    /// Records a failed purge. The host itself stays registered so the operator can retry.
    fn fail_purge_job(&self, id: &str, message: &str) -> Result<()> {
        self.update(|state| {
            let job = state
                .host_jobs
                .get_mut(id)
                .ok_or_else(|| NotFound("host job".into()))?;
            job.status = JobStatus::Failed;
            job.error = Some(message.into());
            job.finished_at = Some(now());
            if let Some(stored) = state.hosts.get_mut(&job.host_id) {
                stored.host.status = crate::remote::HostStatus::Failed;
                stored.host.error = Some(message.into());
                stored.host.active_job = None;
                stored.host.updated_at = now();
            }
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
    check_artifact_build(&artifact.info.platform(), &artifact.info.build_id)
}

fn check_artifact_build(arch: &str, build_id: &str) -> Result<()> {
    if build_id == crate::BUILD_ID {
        return Ok(());
    }
    if build_id.is_empty() {
        return Err(anyhow::anyhow!(
            "the {arch} farm-agent artifact was built before build stamps existed; rebuild it with `task agent-build` and restart the coordinator, which reinstalls the agent"
        ));
    }
    Err(anyhow::anyhow!(
        "the {arch} farm-agent artifact was built from {build_id} but this coordinator is {daemon}; rebuild it with `task agent-build` and restart the coordinator, which reinstalls the agent",
        daemon = crate::BUILD_ID
    ))
}

/// Test hook: the same check without a real artifact file.
#[cfg(test)]
pub(crate) fn artifact_built_by_daemon_for_test(arch: &str, build_id: &str) -> Result<()> {
    check_artifact_build(arch, build_id)
}

fn token_digest(token: &str) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The preparation step for the msb a host reports. A newer msb is refused: going back
/// needs msb's own state downgrade.
fn msb_install_step(found: Option<&str>) -> Result<String> {
    Ok(match found {
        Some(version) if msb_outdated(version) => format!(
            "Upgrading msb {version} to {MSB_VERSION} and libkrunfw (release checksum verified)"
        ),
        Some(version) if version != MSB_VERSION => anyhow::bail!(
            "msb {version} is newer than {MSB_VERSION}, which this coordinator runs; Farm does not downgrade msb"
        ),
        _ => format!(
            "Installing or verifying msb {MSB_VERSION} and libkrunfw (release checksum verified)"
        ),
    })
}
