use super::*;
use crate::remote::{
    AgentHostToken, HostInspection, HostJob, HostJobEvent, HostJobKind, HostKind, HostStatus,
    MSB_VERSION, RegisterAgentHost, RegisterHost, RegisterLocalHost, RemoteHost, StoredHost,
    link::AgentLink,
    msb_outdated,
    protocol::{Operation, Reply, Request, VERSION},
    ssh::Session,
};

/// Separates the items of a "Left on the host" event; the web portal splits on it too. Items
/// may hold ";" and "(...)", so the separator is none of those.
const LEFT_SEPARATOR: &str = " | ";

impl StackboxManager {
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
                annotate_pci_owners(&store.state, &mut host);
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
                            .stackboxes
                            .values()
                            .filter(|f| f.status != StackboxStatus::Deleted)
                            .map(|f| f.memory_on(&host.id))
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
    pub(super) fn stored_host(&self, id: &str) -> Result<StoredHost> {
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
                reachability: None,
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
    /// Registers an SSH host and starts its preparation job (check, msb, kernel, agent). The
    /// registration stands even when that job cannot start; the host's "환경 준비" retries it.
    pub fn register_and_prepare_host(&self, request: RegisterHost) -> Result<RemoteHost> {
        let host = self.register_host(request)?;
        Ok(self.prepare_after_registration(host))
    }
    /// Registers this machine and starts its preparation job, as `register_and_prepare_host` does.
    pub fn register_and_prepare_local_host(
        &self,
        request: RegisterLocalHost,
    ) -> Result<RemoteHost> {
        let host = self.register_local_host(request)?;
        Ok(self.prepare_after_registration(host))
    }
    /// Changes an SSH host's credentials and reruns its preparation; every step of it is
    /// idempotent, so an unchanged install is skipped.
    pub fn update_host_credentials_and_prepare(&self, request: RegisterHost) -> Result<RemoteHost> {
        let host = self.update_host_credentials(request)?;
        Ok(self.prepare_after_registration(host))
    }
    /// Starts the host's preparation and returns the host as the job now leaves it. A host with
    /// an active job, or a stackbox operation on it, stays registered without a new job.
    fn prepare_after_registration(&self, host: RemoteHost) -> RemoteHost {
        match self.start_host_job(&host.id, HostJobKind::Prepare) {
            Ok(_) => self.host(&host.id).unwrap_or(host),
            Err(error) => {
                eprintln!(
                    "host '{}' is registered; its preparation did not start: {error:#}",
                    host.id
                );
                host
            }
        }
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
            reachability: None,
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
    /// (`stackbox agent connect`). No address or credential is stored.
    pub fn register_agent_host(&self, request: RegisterAgentHost) -> Result<AgentHostToken> {
        crate::model::validate_id(&request.id).map_err(|e| bad_request(e.to_string()))?;
        let mut secret = [0u8; 32];
        getrandom::getrandom(&mut secret).map_err(|e| anyhow!("cannot generate a token: {e}"))?;
        let token = format!(
            "stackbox_{}",
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
            reachability: None,
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
                .stackboxes
                .values()
                .any(|f| f.hosts().contains(&id) && f.status != StackboxStatus::Deleted)
            {
                return Err(conflict(
                    "delete all stackboxes on this host before unregistering it",
                ));
            }
            // This removes only coordinator records and encrypted credentials, never remote VMs/files.
            state.hosts.remove(id);
            state.host_jobs.retain(|_, job| job.host_id != id);
            // Snapshot copies there stay on the host, unrecorded, like its files.
            for snapshot in state.snapshots.values_mut() {
                snapshot.copies.retain(|copy| copy.host_id != id);
            }
            Ok(())
        })
    }
    pub fn update_host_credentials(&self, request: RegisterHost) -> Result<RemoteHost> {
        request.validate()?;
        request.credentials.validate()?;
        self.update(|state| {
            self.ensure_accepting()?;
            if state
                .stackboxes
                .values()
                .any(|f| f.hosts().contains(&request.id.as_str()) && f.active_job.is_some())
            {
                return Err(conflict("a stackbox operation is running on this host"));
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
    /// The self-contained script that installs the stackbox-vfio-bind helper on host `id` for
    /// `user`, the account the host's agent runs `sudo -n` as: an SSH host's login user by default.
    /// The script that removes the stackbox-vfio-bind helper and its sudoers rule from host `id`.
    /// It is only shown to the operator; nothing runs it on the host.
    pub fn pci_helper_remove(&self, id: &str) -> Result<crate::remote::pci::PciHelperRemove> {
        self.stored_host(id)?;
        Ok(crate::remote::pci::PciHelperRemove {
            script: crate::remote::pci::render_vfio_helper_remove_script(),
            helper_path: crate::remote::pci::HELPER_PATH.into(),
            sudoers_path: crate::remote::pci::SUDOERS_PATH.into(),
        })
    }
    pub fn pci_helper_install(
        &self,
        id: &str,
        user: Option<&str>,
    ) -> Result<crate::remote::pci::PciHelperInstall> {
        let stored = self.stored_host(id)?;
        let user = match (user, stored.host.kind) {
            (Some(user), _) => user.to_owned(),
            (None, HostKind::Ssh) => stored.host.username.clone(),
            (None, _) => {
                return Err(bad_request(
                    "name the account the host's stackbox-agent runs as with ?user=<name>",
                ));
            }
        };
        if !crate::remote::pci::valid_agent_user(&user) {
            return Err(bad_request(format!(
                "account '{user}' must match [a-z_][a-z0-9_-]*"
            )));
        }
        Ok(crate::remote::pci::PciHelperInstall {
            script: crate::remote::pci::render_vfio_helper_install_script(&user),
            helper_path: crate::remote::pci::HELPER_PATH.into(),
            sudoers_path: crate::remote::pci::SUDOERS_PATH.into(),
        })
    }
    pub fn start_host_job(&self, id: &str, kind: HostJobKind) -> Result<HostJob> {
        self.start_job(id, kind, false, false, false, None)
    }
    /// Binds PCI device `bdf` of host `id` to `vfio-pci` through the host's helper. Refused with a
    /// conflict while a stackbox holds the device, and with a bad request when the host reports
    /// it protected or has no helper installed. The job's inspection replaces the host's.
    pub fn bind_host_pci(&self, id: &str, bdf: &str) -> Result<HostJob> {
        let bdf = normalize_pci_request(bdf)?;
        self.start_job(id, HostJobKind::PciBind, false, false, false, Some(bdf))
    }
    /// Gives PCI device `bdf` of host `id` back to its host driver, under the same checks as a bind
    /// except the protection: a device the helper holds as vfio-pci can always go back.
    pub fn unbind_host_pci(&self, id: &str, bdf: &str) -> Result<HostJob> {
        let bdf = normalize_pci_request(bdf)?;
        self.start_job(id, HostJobKind::PciUnbind, false, false, false, Some(bdf))
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
                        "host {} runs another build's stackbox-agent and no current artifact is loaded for it; run `task agent-build` and restart",
                        host.id
                    );
                }
            }
        }
        let mut jobs = Vec::new();
        for id in stale {
            match self.start_host_job(&id, HostJobKind::InstallAgent) {
                Ok(job) => jobs.push(job),
                Err(error) => eprintln!("cannot reinstall the stackbox-agent on {id}: {error:#}"),
            }
        }
        Ok(jobs)
    }
    /// Purges the host; `delete_snapshots` also deletes the snapshots whose original it keeps,
    /// with their copies on other hosts. Copies of other snapshots on it are forgotten either
    /// way, and originals left without the flag stay recorded until the host returns.
    /// `remote_files` also removes what stackbox put on the host outside its VMs (the golden
    /// groups, msb when stackbox installed it, and the stackbox directory); see `remove_host_files`.
    /// Without `all_vms` the purge removes only the VMs this coordinator reserved on the host, and
    /// the shared files stay while another coordinator's VMs remain there. With it, the purge
    /// removes every VM on the host, whichever coordinator reserved it.
    pub fn start_purge(
        &self,
        id: &str,
        delete_snapshots: bool,
        remote_files: bool,
        all_vms: bool,
    ) -> Result<HostJob> {
        self.start_job(
            id,
            HostJobKind::Purge,
            delete_snapshots,
            remote_files,
            all_vms,
            None,
        )
    }
    fn start_job(
        &self,
        id: &str,
        kind: HostJobKind,
        delete_snapshots: bool,
        remote_files: bool,
        all_vms: bool,
        pci_bdf: Option<String>,
    ) -> Result<HostJob> {
        let job = self.update(|state| {
            if !self.is_accepting() {
                return Err(conflict("daemon is draining"));
            }
            if let Some(bdf) = pci_bdf.as_deref() {
                ensure_pci_request(state, id, bdf, kind)?;
            }
            if state
                .stackboxes
                .values()
                .any(|f| f.hosts().contains(&id) && f.active_job.is_some())
            {
                return Err(conflict("a stackbox operation is running on this host"));
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
                remote_files,
                all_vms,
                pci_bdf,
            };
            host.active_job = Some(job.id.clone());
            host.error = None;
            host.status = match kind {
                HostJobKind::Check | HostJobKind::PciBind | HostJobKind::PciUnbind => {
                    HostStatus::Checking
                }
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
    /// Removes everything this coordinator put on the host: its stackboxes' VMs, any VM left in the
    /// agent's registry, and then the host record itself. Sandboxes the agent does not own stay.
    async fn purge_host(&self, job: &HostJob) -> Result<HostInspection> {
        let host_id = job.host_id.clone();
        let stored = self.stored_host(&host_id)?;
        let stackboxes: Vec<String> = self
            .list()?
            .into_iter()
            .filter(|stackbox| stackbox.hosts().contains(&host_id.as_str()))
            .map(|stackbox| stackbox.id)
            .collect();
        if !stackboxes.is_empty() {
            self.host_event(
                &job.id,
                &format!("Deleting {} stackbox(s) and their VMs", stackboxes.len()),
            )?;
        }
        for stackbox in &stackboxes {
            // This job owns the host's active_job slot, so bypass that guard.
            let delete = self.delete_stackbox(stackbox, true, false)?;
            let outcome = self.wait(&delete.id).await?;
            if outcome.status != crate::model::JobStatus::Succeeded {
                anyhow::bail!(
                    "deleting stackbox {stackbox} failed: {}; the host was left registered",
                    outcome.error.unwrap_or_else(|| "unknown error".into())
                );
            }
        }
        if job.delete_snapshots {
            // Only the originals this host keeps; their copies elsewhere go with them.
            let local = stored.host.kind == HostKind::Local;
            let snapshots: Vec<_> = self
                .snapshots()?
                .into_iter()
                .filter(|snapshot| match &snapshot.host_id {
                    Some(origin) => *origin == host_id,
                    None => local,
                })
                .collect();
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
        // Copies of other hosts' snapshots go too; unregistering forgets any left.
        let copies: Vec<String> = self
            .snapshots()?
            .into_iter()
            .filter(|snapshot| snapshot.copies.iter().any(|copy| copy.host_id == host_id))
            .map(|snapshot| snapshot.id)
            .collect();
        if !copies.is_empty() {
            self.host_event(
                &job.id,
                &format!("Removing copies of {} snapshot(s)", copies.len()),
            )?;
        }
        for snapshot in &copies {
            if let Err(error) = self.remove_snapshot_copy(snapshot, &host_id).await {
                self.host_event(
                    &job.id,
                    &format!("The copy of snapshot {snapshot} stays on the host: {error:#}"),
                )?;
            }
        }

        self.host_event(
            &job.id,
            if job.all_vms {
                "Removing every VM reservation the agent holds, including other stackbox coordinators'"
            } else {
                "Removing VM reservations the agent still holds for this coordinator"
            },
        )?;
        let link = self
            .host_link(&self.stored_host(&host_id)?)
            .context("host has no installed agent to clean up with")?;
        let operation = if job.all_vms {
            Operation::Purge
        } else {
            Operation::PurgeOwn
        };
        let reply = link
            .call(
                Request {
                    version: VERSION,
                    coordinator_id: self.coordinator_id()?,
                    operation,
                },
                None,
            )
            .await
            .map_err(|error| {
                anyhow::anyhow!(
                    "the host's agent could not run the cleanup ({error:#}); install the current agent from Hosts and remove the host again"
                )
            })?;
        let (purged, kept_for_others) = match reply {
            Reply::Purged {
                stackboxes,
                kept_for_others,
            } => (stackboxes, kept_for_others),
            _ => anyhow::bail!("unexpected remote purge response"),
        };
        self.host_event(&job.id, &format!("Removed {purged} reserved VM(s)"))?;
        if kept_for_others > 0 {
            self.host_event(
                &job.id,
                &format!(
                    "Kept {kept_for_others} VM reservation(s) of other stackbox coordinators on the host"
                ),
            )?;
        }
        if job.remote_files {
            // The shared files only go when every VM on the host went with this purge.
            let shared = job.all_vms && kept_for_others == 0;
            self.remove_host_files(job, &stored, shared, kept_for_others)
                .await?;
        }
        self.host_event(&job.id, "Unregistering the host")?;
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

    /// Tell how the host gets its golden image, which preparation leaves to the first stackbox.
    fn golden_event(&self, job: &str, inspection: &HostInspection) -> Result<()> {
        let golden = crate::stackbox::golden::current();
        self.host_event(
            job,
            &if inspection.golden_image_available {
                format!("Golden image {golden} is built")
            } else {
                format!(
                    "Golden image {golden} is not built yet; the first stackbox builds it from {} with msb",
                    crate::stackbox::golden::DEFAULT_OS.base_image
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
    /// stackbox created on it is about to start. Returns whether it inspected the host.
    pub(super) async fn refresh_ssh_inspection(&self, host_id: &str, max_age: u64) -> Result<bool> {
        let stored = self.stored_host(host_id)?;
        let Some(previous) = stored.host.inspection.as_ref() else {
            anyhow::bail!("host '{host_id}' has never been inspected; check it under Hosts");
        };
        if stored.host.kind != HostKind::Ssh || now().saturating_sub(previous.checked_at) <= max_age
        {
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
        let same_kernel = previous.msb_version == inspection.msb_version
            && previous.custom_kernel == inspection.custom_kernel;
        inspection.keep_verification(previous, same_kernel);
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
        if matches!(job.kind, HostJobKind::PciBind | HostJobKind::PciUnbind) {
            return self.run_pci_job(&job, &stored).await;
        }
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
        // The mtty device and the GPU probe are the kernel's, so they stand while the host
        // boots the same one.
        if let Some(previous) = stored.host.inspection.as_ref() {
            let same_kernel = previous.msb_version == inspection.msb_version
                && previous.custom_kernel == inspection.custom_kernel;
            inspection.keep_verification(previous, same_kernel);
        }
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
                self.host_event(id, "Checking the installed Stackbox execution agent")?;
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
                if !matches!(result, Ok(crate::remote::protocol::Reply::Hello { version: crate::remote::protocol::VERSION, ref arch, supports_single_vm: true, ref build_id, .. }) if arch == &agent.artifact.arch && build_id == crate::BUILD_ID)
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
                        "the installed Stackbox execution agent was built from a different revision than this coordinator ({build}); run `task agent-build` and install the agent again",
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
        // msb is now the one this build pairs with, so the kernel built for it can be used.
        self.install_ssh_kernel(&ssh, id, &inspection).await?;
        inspection = ssh.inspect().await?;
        self.host_inspection(&job.host_id, inspection.clone())?;
        self.host_event(
            id,
            "Booting a 512 MiB verification VM, executing a command and removing it",
        )?;
        let gpu = ssh
            .smoke(
                id,
                inspection.custom_kernel.as_deref(),
                inspection.custom_kernel_reason.as_deref(),
            )
            .await?;
        inspection = ssh.inspect().await?;
        // The verification boot is the only evidence of the kernel's GPU features: the shell
        // inspection reads the installation, not a booted kernel.
        inspection.mtty_available = gpu.mtty_available();
        inspection.gpu_kernel = Some(gpu);
        self.host_inspection(&job.host_id, inspection.clone())?;
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
                    "No {} stackbox-agent of this build is loaded or published; preparation finished without installing the execution agent",
                    inspection.platform()
                ),
            )?;
        }
        Ok(inspection)
    }
    /// Runs the helper's bind or unbind for the job's device, then stores the host's inspection
    /// as the helper left it. The inspection keeps the verification evidence it still stands on.
    async fn run_pci_job(&self, job: &HostJob, stored: &StoredHost) -> Result<HostInspection> {
        let id = job.id.as_str();
        let bdf = job.pci_bdf.clone().context("PCI job names no device")?;
        let bind = matches!(job.kind, HostJobKind::PciBind);
        let verb = if bind { "bind" } else { "unbind" };
        let previous = stored.host.inspection.clone();
        let mut inspection = if stored.host.kind == HostKind::Ssh {
            self.host_event(
                id,
                &format!("Running the stackbox-vfio-bind helper: {verb} {bdf}"),
            )?;
            let credentials = self.0.vault.unseal(
                stored
                    .credentials
                    .as_ref()
                    .context("remote host has no credentials")?,
                &stored.aad(),
            )?;
            let ssh = Session::connect(&stored.host, credentials).await?;
            let output = ssh
                .run_pci_helper(&crate::remote::pci::helper_command(verb, &bdf), verb, &bdf)
                .await?;
            crate::remote::pci::ensure_reported(&output, verb, &bdf)?;
            ssh.inspect().await?
        } else {
            self.host_event(
                id,
                &format!("Running the stackbox-vfio-bind helper: {verb} {bdf}"),
            )?;
            let operation = if bind {
                Operation::PciBind { bdf: bdf.clone() }
            } else {
                Operation::PciUnbind { bdf: bdf.clone() }
            };
            let request = Request {
                version: VERSION,
                coordinator_id: self.coordinator_id()?,
                operation,
            };
            match self.host_link(stored)?.call(request, None).await? {
                Reply::Inspection { inspection } => *inspection,
                _ => anyhow::bail!("unexpected stackbox-agent response to the PCI helper"),
            }
        };
        if let Some(previous) = previous.as_ref() {
            carry_verification(previous, &mut inspection);
        }
        self.host_inspection(&job.host_id, inspection.clone())?;
        self.host_event(id, &format!("The helper confirmed {verb} of {bdf}"))?;
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
        self.host_event(id, "Checking the host's stackbox agent")?;
        let hello = call(Operation::Hello).await?;
        anyhow::ensure!(
            matches!(hello, Reply::Hello { version: VERSION, ref build_id, .. } if build_id == crate::BUILD_ID),
            "the host's stackbox agent was built from other sources than this coordinator ({}); update it",
            crate::BUILD_ID
        );
        self.host_event(
            id,
            "Inspecting the host: OS, architecture, hypervisor, memory, disk, msb and the golden image",
        )?;
        let inspect = || async {
            match call(Operation::Inspect).await? {
                Reply::Inspection { inspection } => Ok::<_, anyhow::Error>(*inspection),
                _ => anyhow::bail!("unexpected stackbox-agent inspection response"),
            }
        };
        let mut inspection = inspect().await?;
        // A process restart forgets the last verification VM; the evidence stands while the
        // kernel it verified has not changed.
        if let Some(previous) = stored.host.inspection.as_ref() {
            inspection
                .keep_verification(previous, inspection.custom_kernel == previous.custom_kernel);
        }
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
            // The installer rewrites ~/.microsandbox/lib on the host. The host's own
            // inspection says which kernel it boots, which is what decides whether the
            // installer would replace it: a coordinator started without a custom kernel
            // must still leave an agent host that has one alone.
            let replaced = inspection.custom_kernel.as_deref().is_some_and(|path| {
                crate::kernel::install_would_replace(std::path::Path::new(path))
            });
            if replaced {
                self.host_event(
                    id,
                    &format!(
                        "Skipping the msb install: it would replace the custom kernel this host boots at {} (keep STACKBOX_LIBKRUNFW_PATH outside ~/.microsandbox/lib on the host)",
                        inspection.custom_kernel.as_deref().unwrap_or_default()
                    ),
                )?;
            } else {
                self.host_event(id, &msb_install_step(inspection.msb_version.as_deref())?)?;
                call(Operation::InstallMsb).await?;
                inspection = inspect().await?;
                self.host_inspection(&job.host_id, inspection.clone())?;
            }
            anyhow::ensure!(
                inspection.msb_compatible,
                "{}",
                match inspection.custom_kernel.as_deref() {
                    Some(path) if replaced => format!(
                        "msb {MSB_VERSION} is not installed on the host, but installing it would replace the custom kernel at {path}; install msb {MSB_VERSION} by hand on the host first"
                    ),
                    _ => format!(
                        "msb {MSB_VERSION} is still not installed on the host after the install step (it reports {}); install it there, then check and prepare the host again",
                        inspection.msb_version.as_deref().unwrap_or("no msb")
                    ),
                }
            );
        }
        self.install_agent_kernel(job, stored, &inspection).await?;
        inspection = inspect().await?;
        self.host_inspection(&job.host_id, inspection.clone())?;
        self.host_event(
            id,
            "Booting a 512 MiB verification VM, executing a command and removing it",
        )?;
        // The verification boot reports where it left the host, including whether that boot's
        // kernel exposed the mtty device. Asking the agent again instead would lose it: the
        // local agent is a new process per request (`stackbox host local`, docs/gpu.md).
        let verified = match call(Operation::VerifyVm).await? {
            Reply::Inspection { inspection } => *inspection,
            // An agent built before its verification reply carried the inspection.
            Reply::Done => inspect().await?,
            _ => anyhow::bail!("unexpected stackbox-agent response"),
        };
        inspection = verified;
        anyhow::ensure!(
            inspection.runtime_ready(),
            "the host's prerequisites changed during preparation; check it and retry"
        );
        inspection.vm_boot_verified = true;
        self.host_event(id, "VM boot, command execution and cleanup verified")?;
        self.golden_event(id, &inspection)?;
        Ok(inspection)
    }
    /// The stackbox kernel for a host of this platform: the one built under `dir` when there is
    /// one, else the one stackbox publishes for it, downloaded and cached under the state
    /// directory. `None` when neither has a kernel for the platform.
    async fn stackbox_kernel(
        &self,
        dir: Option<&std::path::Path>,
        os: &str,
        arch: &str,
    ) -> Result<Option<crate::kernel::Artifact>> {
        if let Some(dir) = dir
            && let Some(artifact) = crate::kernel::artifact_for(dir, os, arch)?
        {
            return Ok(Some(artifact));
        }
        let Some(url) = self.0.options.kernel_release_url.as_deref() else {
            return Ok(None);
        };
        // The bucket names macOS `darwin`, as the install script does.
        let bucket_os = if os == "macos" { "darwin" } else { os };
        let cache = self.0.options.state_dir.join("libkrunfw-cache");
        crate::kernel::fetch_published(url, bucket_os, arch, MSB_VERSION, &cache).await
    }
    /// Install the stackbox kernel on an SSH host before its verification boot, under
    /// `~/.local/share/stackbox/libkrunfw`. A missing artifact is not a failure: the host then
    /// boots the stock microsandbox kernel, and preparation records no mtty device.
    async fn install_ssh_kernel(
        &self,
        ssh: &Session,
        job: &str,
        inspection: &HostInspection,
    ) -> Result<()> {
        let dir = self.0.options.libkrunfw_dir.clone();
        match self
            .stackbox_kernel(dir.as_deref(), &inspection.os, &inspection.arch)
            .await
        {
            Ok(Some(artifact)) => {
                self.host_event(
                    job,
                    &format!(
                        "Installing the stackbox kernel {} for {}-{} (built for msb {})",
                        artifact.id, inspection.os, inspection.arch, artifact.msb
                    ),
                )?;
                ssh.install_kernel(&artifact).await?;
            }
            Ok(None) => {
                self.host_event(
                    job,
                    &format!(
                        "No stackbox kernel for {}-{}{}; this host boots the stock microsandbox kernel (mtty GPUs are unavailable)",
                        inspection.os, inspection.arch, kernel_source(dir.as_deref())
                    ),
                )?;
            }
            Err(error) => {
                self.host_event(
                    job,
                    &format!(
                        "The stackbox kernel was not installed: {error:#}; this host boots the stock microsandbox kernel"
                    ),
                )?;
            }
        }
        Ok(())
    }
    /// Install the built stackbox kernel on this machine before its verification boot. Only the
    /// local host shares this process's home; a dial-in host's agent boots a kernel installed
    /// there or named by `STACKBOX_LIBKRUNFW_PATH`.
    async fn install_agent_kernel(
        &self,
        job: &HostJob,
        stored: &StoredHost,
        inspection: &HostInspection,
    ) -> Result<()> {
        if stored.host.kind != HostKind::Local {
            if inspection.custom_kernel.is_none() {
                let note = inspection
                    .custom_kernel_reason
                    .as_deref()
                    .map(|reason| format!(" ({reason})"))
                    .unwrap_or_default();
                self.host_event(
                    &job.id,
                    &format!(
                        "The host's agent boots the stock microsandbox kernel{note}; install stackbox's kernel on it, or set STACKBOX_LIBKRUNFW_PATH there, for mtty GPUs"
                    ),
                )?;
            }
            return Ok(());
        }
        let dir = self.0.options.libkrunfw_dir.clone();
        let artifact = match self
            .stackbox_kernel(dir.as_deref(), &inspection.os, &inspection.arch)
            .await
        {
            Ok(Some(artifact)) => artifact,
            Ok(None) => {
                self.host_event(
                    &job.id,
                    &format!(
                        "No stackbox kernel for {}-{}{}; this host boots the stock microsandbox kernel (mtty GPUs are unavailable)",
                        inspection.os, inspection.arch, kernel_source(dir.as_deref())
                    ),
                )?;
                return Ok(());
            }
            Err(error) => {
                self.host_event(
                    &job.id,
                    &format!(
                        "The stackbox kernel was not installed: {error:#}; this host boots the stock microsandbox kernel"
                    ),
                )?;
                return Ok(());
            }
        };
        let home = crate::kernel::home().context("the local host has no home directory")?;
        let id = artifact.id.clone();
        let msb = artifact.msb.clone();
        let installed =
            tokio::task::spawn_blocking(move || crate::kernel::install(&artifact, &home))
                .await
                .context("the stackbox kernel installation task failed")??;
        self.host_event(
            &job.id,
            &format!(
                "Installed the stackbox kernel {id} at {} (built for msb {msb})",
                installed.display()
            ),
        )?;
        Ok(())
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
                &format!("Fetching the published {platform} stackbox-agent"),
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
            "Uploading stackbox-agent, verifying its SHA256 and checking the protocol",
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
    /// Removes what stackbox left on the host once its VMs are gone. The shared files (the golden
    /// snapshot groups, msb and `~/.local/share/stackbox`) go only when `shared`: every VM on the host
    /// went with this purge. Otherwise another coordinator's VMs may still use them, so they stay,
    /// and the job names them as left. When `shared`, the golden groups go through the agent, under the
    /// golden build lock, then over SSH msb and the directory; each step that fails or is out of reach
    /// is recorded on the job with what to remove by hand. Nothing here stops the unregistration: the
    /// VMs are already gone, and the coordinator must forget the host.
    async fn remove_host_files(
        &self,
        job: &HostJob,
        stored: &StoredHost,
        shared: bool,
        kept_for_others: usize,
    ) -> Result<()> {
        let (left, by_hand) = if shared {
            let left = self.remove_shared_files(job, stored).await?;
            let by_hand = if stored.host.kind == HostKind::Ssh {
                ". To remove the directory by hand, run on the host: rm -rf ~/.local/share/stackbox"
            } else {
                ""
            };
            (left, by_hand)
        } else {
            let reason = if kept_for_others > 0 {
                format!(
                    "golden snapshot groups, msb and ~/.local/share/stackbox (kept: {kept_for_others} VM reservation(s) of other stackbox coordinators still use this host)"
                )
            } else {
                "golden snapshot groups, msb and ~/.local/share/stackbox (kept: other stackboxes' VMs were not asked to go; choose to delete all VMs on the host to remove them)".into()
            };
            (vec![reason], "")
        };
        if !left.is_empty() {
            self.host_event(
                &job.id,
                &format!("Left on the host: {}{by_hand}", left.join(LEFT_SEPARATOR)),
            )?;
        }
        Ok(())
    }

    /// The shared files' removal, step by step: see `remove_host_files`.
    async fn remove_shared_files(&self, job: &HostJob, stored: &StoredHost) -> Result<Vec<String>> {
        let mut left: Vec<String> = Vec::new();
        match self.host_link(stored) {
            Ok(link) => {
                let reply = link
                    .call(
                        Request {
                            version: VERSION,
                            coordinator_id: self.coordinator_id()?,
                            operation: Operation::RemoveGolden,
                        },
                        None,
                    )
                    .await;
                match reply {
                    Ok(Reply::GoldenRemoved { groups }) => self.host_event(
                        &job.id,
                        &format!("Removed {groups} golden snapshot group(s)"),
                    )?,
                    Ok(_) => left.push(
                        "golden snapshot groups (the agent gave an unexpected reply; check the host's msb snapshot groups named stackbox-golden-*)".into(),
                    ),
                    Err(error) => left.push(format!(
                        "golden snapshot groups ({error:#}); check the host's msb snapshot groups named stackbox-golden-*"
                    )),
                }
            }
            Err(error) => left.push(format!("golden snapshot groups ({error:#})")),
        }
        match stored.host.kind {
            HostKind::Ssh => {
                self.host_event(&job.id, "Removing stackbox's files from the host")?;
                match self.ssh_session(stored).await {
                    Ok(ssh) => {
                        let (outcome, problem) = ssh.remove_stackbox_files().await;
                        for item in &outcome.removed {
                            self.host_event(&job.id, &format!("Removed {item}"))?;
                        }
                        for item in &outcome.kept {
                            left.push(format!("{item} (kept on purpose)"));
                        }
                        for item in &outcome.failed {
                            left.push(format!("{item} (failed)"));
                        }
                        if let Some(problem) = problem {
                            left.push(format!(
                                "the rest of the host's files ({problem}); the cleanup may have run partly"
                            ));
                        }
                    }
                    Err(error) => left.push(format!(
                        "the host's files (could not connect for the cleanup: {error:#})"
                    )),
                }
            }
            HostKind::Local => left.push(
                "this machine's msb and ~/.local/share/stackbox (this coordinator's own host; kept on purpose)".into(),
            ),
            HostKind::Agent => left.push(
                "the host's msb and ~/.local/share/stackbox (a dial-in host has no SSH for the cleanup; remove them on the host)".into(),
            ),
        }
        Ok(left)
    }
    /// Opens an SSH session with the host's pinned key and stored credentials.
    async fn ssh_session(&self, stored: &StoredHost) -> Result<Session> {
        let credentials = self.0.vault.unseal(
            stored
                .credentials
                .as_ref()
                .context("remote host has no credentials")?,
            &stored.aad(),
        )?;
        Session::connect(&stored.host, credentials).await
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

/// Normalizes a PCI address from a request; a malformed one is a bad request.
fn normalize_pci_request(bdf: &str) -> Result<String> {
    crate::model::normalize_pci_bdf(bdf).map_err(|error| bad_request(format!("{error:#}")))
}

/// Checks a bind or unbind of `bdf` on host `host_id` against the state, before the job exists:
/// a live stackbox holding the device is a conflict; the host must have the helper and the device
/// among its candidates; a bind refuses a device the host reports protected.
fn ensure_pci_request(state: &State, host_id: &str, bdf: &str, kind: HostJobKind) -> Result<()> {
    let stored = state
        .hosts
        .get(host_id)
        .ok_or_else(|| NotFound(format!("host '{host_id}'")))?;
    if let Some(stackbox) = state.stackboxes.values().find(|stackbox| {
        stackbox.status != StackboxStatus::Deleted
            && stackbox.host_of("compute-1") == Some(host_id)
            && stackbox
                .plan
                .as_ref()
                .is_some_and(|plan| plan.pci_devices.iter().any(|device| device.as_str() == bdf))
    }) {
        return Err(conflict(format!(
            "PCI device '{bdf}' on host '{host_id}' is held by stackbox '{}'; delete that stackbox first",
            stackbox.id
        )));
    }
    let inspection = stored.host.inspection.as_ref().ok_or_else(|| {
        bad_request(format!(
            "host '{host_id}' has not been inspected yet; check it under Hosts first"
        ))
    })?;
    if let Some(version) = inspection.pci_bind_helper_version
        && !crate::remote::pci::helper_matches(Some(version))
    {
        return Err(bad_request(crate::remote::pci::outdated_helper_message(
            version,
        )));
    }
    if !inspection.pci_bind_helper_available {
        return Err(bad_request(format!(
            "host '{host_id}' has no stackbox-vfio-bind helper; install it with the command under Hosts (GET /api/v1/hosts/{host_id}/pci/helper-install), then check the host again"
        )));
    }
    let candidate = inspection
        .pci_candidates
        .iter()
        .find(|candidate| candidate.bdf == bdf)
        .ok_or_else(|| {
            NotFound(format!(
                "PCI device '{bdf}' on host '{host_id}' (check the host to refresh its devices)"
            ))
        })?;
    if matches!(kind, HostJobKind::PciBind)
        && let Some(reason) = candidate.protected_reason.as_deref()
    {
        return Err(bad_request(format!(
            "PCI device '{bdf}' on host '{host_id}' is protected ({reason}) and cannot be bound"
        )));
    }
    Ok(())
}

/// Marks the candidates of `host` that a live stackbox holds, so the portal can name the owner.
/// A held device cannot be bound, so its protection reads `bound_to_stackbox` unless the host
/// already gave a reason.
fn annotate_pci_owners(state: &State, host: &mut RemoteHost) {
    let Some(inspection) = host.inspection.as_mut() else {
        return;
    };
    for candidate in &mut inspection.pci_candidates {
        let owner = state.stackboxes.values().find(|stackbox| {
            stackbox.status != StackboxStatus::Deleted
                && stackbox.host_of("compute-1") == Some(host.id.as_str())
                && stackbox
                    .plan
                    .as_ref()
                    .is_some_and(|plan| plan.pci_devices.contains(&candidate.bdf))
        });
        if let Some(owner) = owner {
            candidate.owning_stackbox = Some(owner.id.clone());
            candidate.bindable = false;
            candidate
                .protected_reason
                .get_or_insert_with(|| "bound_to_stackbox".into());
        }
    }
}

/// What a helper-verified inspection keeps from the last verification of this host: the same
/// rules as a read-only check of an SSH host, kept with the kernel it verified.
fn carry_verification(previous: &HostInspection, inspection: &mut HostInspection) {
    inspection.vm_boot_verified = inspection.runtime_ready()
        && previous.vm_boot_verified
        && previous.msb_version == inspection.msb_version;
    let same_kernel = previous.msb_version == inspection.msb_version
        && previous.custom_kernel == inspection.custom_kernel;
    inspection.keep_verification(previous, same_kernel);
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
            "the {arch} stackbox-agent artifact was built before build stamps existed; rebuild it with `task agent-build` and restart the coordinator, which reinstalls the agent"
        ));
    }
    Err(anyhow::anyhow!(
        "the {arch} stackbox-agent artifact was built from {build_id} but this coordinator is {daemon}; rebuild it with `task agent-build` and restart the coordinator, which reinstalls the agent",
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
        Some(version) if msb_outdated(version) => {
            format!("Upgrading msb {version} to {MSB_VERSION} (release checksum verified)")
        }
        Some(version) if version != MSB_VERSION => anyhow::bail!(
            "msb {version} is newer than {MSB_VERSION}, which this coordinator runs; Stackbox does not downgrade msb"
        ),
        _ => format!("Installing or verifying msb {MSB_VERSION} (release checksum verified)"),
    })
}

/// Where a host's stackbox kernel was looked for, for the event that reports it is missing.
fn kernel_source(dir: Option<&std::path::Path>) -> String {
    match dir {
        Some(dir) => format!(" in {}", dir.display()),
        None => " built or published".into(),
    }
}
