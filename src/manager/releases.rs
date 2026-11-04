//! Farms created by OpenStack release: the control VM from a control template, each compute
//! from a compute template of the same release when one can boot on its host, otherwise
//! installed clean. Without a control template the farm is installed clean, which only the
//! release this coordinator deploys can be. The steps run as ordinary jobs, one at a time,
//! so a step that fails leaves a farm the usual jobs can retry, expand or delete.

use super::{FarmManager, NotFound, bad_request, conflict, new_job, store::State};
use crate::{
    model::*,
    remote::HostKind,
    s3::catalog::{ArchiveMetadata, TemplateQuery},
};
use anyhow::{Context, Result, anyhow, bail};
use std::collections::{BTreeMap, BTreeSet};

/// A template the plan picked.
#[derive(Clone)]
enum Pick {
    Local(Box<SnapshotRecord>),
    Bucket {
        bucket: String,
        metadata: Box<ArchiveMetadata>,
        /// The snapshot ID it is imported as.
        id: String,
    },
}
impl Pick {
    fn id(&self) -> &str {
        match self {
            Self::Local(snapshot) => &snapshot.id,
            Self::Bucket { id, .. } => id,
        }
    }
    fn components(&self) -> &[String] {
        match self {
            Self::Local(snapshot) => &snapshot.plan.components,
            Self::Bucket { metadata, .. } => &metadata.components,
        }
    }
    fn planned(&self, node: String, host_id: String, reason: String) -> PlannedNode {
        let (source, bucket, key) = match self {
            Self::Local(_) => (NodeSource::Template, None, None),
            Self::Bucket {
                bucket, metadata, ..
            } => (
                NodeSource::Bucket,
                Some(bucket.clone()),
                Some(metadata.key.clone()),
            ),
        };
        PlannedNode {
            node,
            host_id,
            source,
            snapshot_id: Some(self.id().into()),
            bucket,
            key,
            reason,
        }
    }
    fn describe(&self) -> String {
        match self {
            Self::Local(snapshot) => format!("template '{}'", snapshot.id),
            Self::Bucket {
                bucket, metadata, ..
            } => format!("{} in bucket '{bucket}'", metadata.key),
        }
    }
}

/// Templates of one role found for a release, newest first: kept here, then in buckets.
struct Found {
    local: Vec<SnapshotRecord>,
    bucket: Vec<(String, ArchiveMetadata)>,
    /// Ones of the release and architecture that run other services.
    other_services: usize,
}

fn sorted(components: &[String]) -> Vec<String> {
    let mut components = components.to_vec();
    components.sort();
    components
}

/// A snapshot that can start a VM now, as [`super::snapshots::restorable`] and
/// [`super::snapshots::compute_template`] require.
fn ready(snapshot: &SnapshotRecord) -> bool {
    matches!(
        snapshot.status,
        SnapshotStatus::Ready | SnapshotStatus::Exporting
    ) && snapshot.reference.is_some()
}

/// Whether a template can boot a VM on `host`. Templates are kept on the coordinator's
/// local host only; this is the one place that decides it, so a transfer of templates to
/// other hosts changes only this and [`FarmManager::ensure_template_on_host`].
fn template_on_host(state: &State, host: &str) -> std::result::Result<(), String> {
    match state.hosts.get(host).map(|stored| stored.host.kind) {
        Some(HostKind::Local) => Ok(()),
        Some(_) => Err(format!(
            "templates are kept on the local host and are not copied to host '{host}' yet"
        )),
        None => Err(format!("host '{host}' is not registered")),
    }
}

/// Marks an assemble job the daemon stopped during. Its farm and step jobs are left to
/// their own recovery.
pub(super) fn interrupt(job: &mut Job) {
    job.status = JobStatus::Interrupted;
    job.error = Some(
        "daemon stopped during this job; the steps that finished stay, add the remaining computes or delete the farm"
            .into(),
    );
    job.finished_at = Some(now());
    for step in job.steps.iter_mut().filter(|step| step.status.active()) {
        step.status = JobStatus::Interrupted;
    }
}

impl FarmManager {
    /// The releases farms can be created with on `arch` (this host's by default), with the
    /// templates of each kept here and in `bucket`, or in every bucket.
    pub async fn release_catalog(
        &self,
        arch: Option<String>,
        bucket: Option<String>,
    ) -> Result<ReleaseCatalog> {
        let arch = arch.unwrap_or_else(current_arch);
        let mut counts: BTreeMap<String, ReleaseTemplates> = BTreeMap::new();
        let mut add = |release: &str, role: Option<TemplateRole>, local: bool| {
            let counted = counts
                .entry(release.into())
                .or_insert_with(|| ReleaseTemplates {
                    release: release.into(),
                    installable: release == crate::kolla::OPENSTACK_RELEASE
                        && arch == current_arch(),
                    ..Default::default()
                });
            match (role, local) {
                (None, _) => {}
                (Some(TemplateRole::Control), true) => counted.local_control += 1,
                (Some(TemplateRole::Compute), true) => counted.local_compute += 1,
                (Some(TemplateRole::Control), false) => counted.bucket_control += 1,
                (Some(TemplateRole::Compute), false) => counted.bucket_compute += 1,
            }
        };
        for snapshot in self.snapshots()? {
            if let (Some(role), Some(release)) = (snapshot.template, &snapshot.release)
                && snapshot.arch == arch
                && ready(&snapshot)
            {
                add(release, Some(role), true);
            }
        }
        let query = TemplateQuery {
            arch: Some(arch.clone()),
            ..Default::default()
        };
        let (found, unreachable) = self.search_buckets(bucket, &query).await?;
        for (_, metadata) in found {
            if let (Some(role), Some(release)) = (metadata.role, &metadata.release) {
                add(release, Some(role), false);
            }
        }
        // The release this coordinator installs clean is always offered on its architecture.
        if arch == current_arch() {
            add(crate::kolla::OPENSTACK_RELEASE, None, true);
        }
        Ok(ReleaseCatalog {
            arch,
            releases: counts.into_values().rev().collect(),
            unreachable,
        })
    }

    /// Templates `query` matches in `bucket`, or in every registered bucket, newest export
    /// first, and the buckets that could not be listed. A bucket asked for by name must answer.
    async fn search_buckets(
        &self,
        bucket: Option<String>,
        query: &TemplateQuery,
    ) -> Result<(Vec<(String, ArchiveMetadata)>, Vec<String>)> {
        let named = bucket.is_some();
        let ids = match bucket {
            Some(id) => {
                self.bucket(&id)?;
                vec![id]
            }
            None => self.buckets()?.into_iter().map(|b| b.id).collect(),
        };
        let mut found = Vec::new();
        let mut unreachable = Vec::new();
        for id in ids {
            match self.find_templates(&id, query).await {
                Ok(entries) => found.extend(entries.into_iter().map(|m| (id.clone(), m))),
                Err(error) if named => return Err(error),
                Err(error) => unreachable.push(format!("{id}: {error:#}")),
            }
        }
        found.sort_by_key(|(_, m)| std::cmp::Reverse(m.exported_at));
        Ok((found, unreachable))
    }

    /// Templates of `role` for `release` on this architecture, running `components` when
    /// given; `buckets` holds every bucket template of the release.
    fn found(
        &self,
        state: &State,
        release: &str,
        role: TemplateRole,
        components: Option<&[String]>,
        buckets: &[(String, ArchiveMetadata)],
    ) -> Found {
        let arch = current_arch();
        let services = |c: &[String]| components.is_none_or(|wanted| sorted(wanted) == sorted(c));
        let mut local: Vec<_> = state
            .snapshots
            .values()
            .filter(|s| {
                s.template == Some(role)
                    && s.release.as_deref() == Some(release)
                    && s.arch == arch
                    && ready(s)
            })
            .cloned()
            .collect();
        let mut other_services = local
            .iter()
            .filter(|s| !services(&s.plan.components))
            .count();
        local.retain(|s| services(&s.plan.components));
        local.sort_by_key(|s| std::cmp::Reverse(s.created_at));
        let of_role: Vec<_> = buckets
            .iter()
            .filter(|(_, m)| m.role == Some(role))
            .cloned()
            .collect();
        other_services += of_role
            .iter()
            .filter(|(_, m)| !services(&m.components))
            .count();
        Found {
            local,
            bucket: of_role
                .into_iter()
                .filter(|(_, m)| services(&m.components))
                .collect(),
            other_services,
        }
    }

    /// The ID a bucket template is imported as: the snapshot an import of the same archive
    /// that is still running writes, else the archive's own snapshot ID or the first free
    /// `<id>-<n>`.
    fn import_id(
        state: &State,
        bucket: &str,
        metadata: &ArchiveMetadata,
        taken: &mut BTreeSet<String>,
    ) -> Result<String> {
        if let Some(running) = state.snapshots.values().find(|s| {
            s.status == SnapshotStatus::Importing
                && s.transfer
                    .as_ref()
                    .is_some_and(|t| t.bucket == bucket && t.key == metadata.key)
        }) {
            taken.insert(running.id.clone());
            return Ok(running.id.clone());
        }
        let base = &metadata.snapshot_id;
        for n in 1..100 {
            let id = match n {
                1 => base.clone(),
                n => format!("{}-{n}", base.get(..44).unwrap_or(base)),
            };
            if !state.snapshots.contains_key(&id) && taken.insert(id.clone()) {
                return Ok(id);
            }
        }
        Err(conflict(format!(
            "no free snapshot ID to import {} as",
            metadata.key
        )))
    }

    /// A template the request names, checked against the release and services.
    fn preferred(
        state: &State,
        id: &str,
        role: TemplateRole,
        release: &str,
        components: Option<&[String]>,
    ) -> Result<SnapshotRecord> {
        let snapshot = state
            .snapshots
            .get(id)
            .ok_or_else(|| NotFound(format!("snapshot '{id}'")))?;
        if snapshot.template != Some(role) {
            return Err(conflict(format!(
                "snapshot '{id}' is not a {} template",
                role.as_str()
            )));
        }
        if !ready(snapshot) {
            return Err(conflict(format!("snapshot '{id}' is not ready")));
        }
        if snapshot.release.as_deref() != Some(release) {
            return Err(conflict(format!(
                "template '{id}' is OpenStack {}, not {release}",
                snapshot
                    .release
                    .as_deref()
                    .unwrap_or("of an unknown release")
            )));
        }
        if snapshot.arch != current_arch() {
            return Err(conflict(format!(
                "template '{id}' was frozen on {}; this host is {}",
                snapshot.arch,
                current_arch()
            )));
        }
        if components.is_some_and(|c| sorted(c) != sorted(&snapshot.plan.components)) {
            return Err(conflict(format!(
                "template '{id}' runs {}, not the services asked for",
                snapshot.plan.components.join(",")
            )));
        }
        Ok(snapshot.clone())
    }

    /// Which template or clean install makes each VM of the farm, and why.
    pub async fn preview_release(&self, request: &ReleaseCreateRequest) -> Result<ReleasePlan> {
        // Requested services resolved as a multi-node farm's, the way templates record them.
        let requested = if request.components.is_empty() {
            None
        } else {
            Some(
                plan(&PlanRequest {
                    components: request.components.clone(),
                    computes: 1,
                    compute: None,
                })
                .map_err(|e| bad_request(e.to_string()))?
                .components,
            )
        };
        self.check_release_request(request)?;
        let query = TemplateQuery {
            release: Some(request.release.clone()),
            arch: Some(current_arch()),
            ..Default::default()
        };
        let (buckets, unreachable) = self.search_buckets(request.bucket.clone(), &query).await?;
        let mut notes: Vec<String> = unreachable
            .into_iter()
            .map(|error| format!("bucket not searched: {error}"))
            .collect();
        self.read(|state| self.resolve(state, request, requested.as_deref(), &buckets, &mut notes))?
            .map(|mut plan| {
                plan.notes.extend(notes);
                plan
            })
    }

    fn check_release_request(&self, request: &ReleaseCreateRequest) -> Result<()> {
        let bad = |e: anyhow::Error| bad_request(e.to_string());
        validate_id(&request.id).map_err(bad)?;
        if request.id.ends_with(crate::farm::LEGACY_ALL_IN_ONE_SUFFIX) {
            return Err(bad_request(format!(
                "farm id must not end with '{}'",
                crate::farm::LEGACY_ALL_IN_ONE_SUFFIX
            )));
        }
        let release = request.release.trim();
        if release.is_empty()
            || release.len() > 32
            || !release
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b".-_".contains(&c))
        {
            return Err(bad_request(
                "release must be an OpenStack release such as 2026.2",
            ));
        }
        if request.computes > crate::farm::MAX_COMPUTES {
            return Err(bad_request(format!(
                "a farm has at most {} compute VMs",
                crate::farm::MAX_COMPUTES
            )));
        }
        if request.compute_hosts.len() > usize::from(request.computes) {
            return Err(bad_request(format!(
                "{} compute hosts given for {} compute VM(s)",
                request.compute_hosts.len(),
                request.computes
            )));
        }
        let host = request.host_id.as_deref().ok_or_else(|| {
            bad_request("choose a host for the control VM; register the local host or a server under Hosts first")
        })?;
        validate_id(host).map_err(bad)?;
        for host in request
            .compute_hosts
            .iter()
            .filter(|h| !h.trim().is_empty())
        {
            validate_id(host).map_err(bad)?;
        }
        for template in [&request.control_template, &request.compute_template]
            .into_iter()
            .flatten()
        {
            validate_snapshot_id(template).map_err(bad)?;
        }
        if request.owner.as_ref().is_some_and(|o| o.len() > 256) {
            return Err(bad_request("owner must be at most 256 bytes"));
        }
        if request.expires_at.is_some_and(|t| t <= now()) {
            return Err(bad_request("expires_at must be in the future"));
        }
        Ok(())
    }

    fn resolve(
        &self,
        state: &State,
        request: &ReleaseCreateRequest,
        requested: Option<&[String]>,
        buckets: &[(String, ArchiveMetadata)],
        notes: &mut Vec<String>,
    ) -> Result<ReleasePlan> {
        let release = request.release.trim();
        let id = &request.id;
        if state
            .farms
            .get(id)
            .is_some_and(|f| f.status != FarmStatus::Deleted)
        {
            return Err(conflict(format!("farm '{id}' already exists")));
        }
        let control_host = request.host_id.clone().context("no control host")?;
        for host in std::iter::once(&control_host).chain(&request.compute_hosts) {
            if !host.trim().is_empty() && !state.hosts.contains_key(host) {
                return Err(NotFound(format!("host '{host}'")).into());
            }
        }
        let installable = release == crate::kolla::OPENSTACK_RELEASE;
        let mut taken = BTreeSet::new();
        let bucket_pick = |bucket: &String, metadata: &ArchiveMetadata, taken: &mut _| {
            Ok::<_, anyhow::Error>(Pick::Bucket {
                bucket: bucket.clone(),
                metadata: Box::new(metadata.clone()),
                id: Self::import_id(state, bucket, metadata, taken)?,
            })
        };

        // The control VM.
        let found = self.found(state, release, TemplateRole::Control, requested, buckets);
        let (pick, why) = match &request.control_template {
            Some(id) => (
                Some(Pick::Local(Box::new(Self::preferred(
                    state,
                    id,
                    TemplateRole::Control,
                    release,
                    requested,
                )?))),
                "the control template asked for".to_owned(),
            ),
            None => match (found.local.first(), found.bucket.first()) {
                (Some(snapshot), _) => (
                    Some(Pick::Local(Box::new(snapshot.clone()))),
                    newest(found.local.len(), "control", release, "kept here"),
                ),
                (None, Some((bucket, metadata))) => (
                    Some(bucket_pick(bucket, metadata, &mut taken)?),
                    newest(
                        found.bucket.len(),
                        "control",
                        release,
                        "in buckets, none kept here",
                    ),
                ),
                (None, None) => (None, String::new()),
            },
        };
        let control = match pick {
            Some(pick) => match template_on_host(state, &control_host) {
                Ok(()) => Ok(pick),
                Err(why) if request.control_template.is_some() => {
                    return Err(conflict(format!(
                        "control template '{}' cannot start the control VM: {why}; choose the local host for it",
                        pick.id()
                    )));
                }
                Err(why) => Err(format!(
                    "the control VM runs on host '{control_host}', and {why}; it is installed clean (choose the local host to start it from {})",
                    pick.describe()
                )),
            },
            None => Err(format!(
                "no control template of OpenStack {release} for {}{}",
                current_arch(),
                mismatch(found.other_services, "control")
            )),
        };
        let control = match control {
            Ok(pick) => pick,
            Err(why) if installable => {
                return self.clean_plan(request, why, notes);
            }
            Err(why) => {
                return Err(conflict(format!(
                    "{why}; this coordinator installs only OpenStack {} clean",
                    crate::kolla::OPENSTACK_RELEASE
                )));
            }
        };
        let components = control.components().to_vec();
        let control_node = control.planned(
            "control".into(),
            control_host.clone(),
            format!(
                "{why}; the farm starts with this control VM and gains its computes one at a time"
            ),
        );

        // Each compute, with the control template's services.
        let found = self.found(
            state,
            release,
            TemplateRole::Compute,
            Some(&components),
            buckets,
        );
        let (pick, why) = match &request.compute_template {
            Some(id) => (
                Some(Pick::Local(Box::new(Self::preferred(
                    state,
                    id,
                    TemplateRole::Compute,
                    release,
                    Some(&components),
                )?))),
                "the compute template asked for".to_owned(),
            ),
            None => match (found.local.first(), found.bucket.first()) {
                (Some(snapshot), _) => (
                    Some(Pick::Local(Box::new(snapshot.clone()))),
                    newest(found.local.len(), "compute", release, "kept here"),
                ),
                (None, Some((bucket, metadata))) => (
                    Some(bucket_pick(bucket, metadata, &mut taken)?),
                    newest(
                        found.bucket.len(),
                        "compute",
                        release,
                        "in buckets, none kept here",
                    ),
                ),
                (None, None) => (None, String::new()),
            },
        };
        let computes = (1..=request.computes)
            .map(|index| {
                let node = format!("compute-{index}");
                let host = request.host_of(index).unwrap_or_else(|| control_host.clone());
                match &pick {
                    Some(pick) => match template_on_host(state, &host) {
                        Ok(()) => pick.planned(node, host, why.clone()),
                        Err(reason) => clean(
                            node,
                            host,
                            format!("{} cannot boot it: {reason}", pick.describe()),
                        ),
                    },
                    None => clean(
                        node,
                        host,
                        format!(
                            "no compute template of OpenStack {release} with the control template's services{}",
                            mismatch(found.other_services, "compute")
                        ),
                    ),
                }
            })
            .collect();
        Ok(ReleasePlan {
            farm_id: id.clone(),
            release: release.into(),
            arch: current_arch(),
            components,
            control: control_node,
            computes,
            notes: std::mem::take(notes),
        })
    }

    /// The plan of a farm installed clean with its computes, as `POST /farms` does.
    fn clean_plan(
        &self,
        request: &ReleaseCreateRequest,
        why: String,
        notes: &mut Vec<String>,
    ) -> Result<ReleasePlan> {
        let control_host = request.host_id.clone().context("no control host")?;
        let farm = CreateRequest {
            components: request.components.clone(),
            computes: request.computes,
            ..self.clean_request(request)
        };
        let planned = farm.validate().map_err(|e| bad_request(e.to_string()))?;
        if request.compute_template.is_some() {
            return Err(conflict(
                "a compute template boots a compute beside a control VM from a control template; without one the computes are installed clean",
            ));
        }
        let node = if planned.multinode() {
            "control"
        } else {
            "all-in-one"
        };
        Ok(ReleasePlan {
            farm_id: request.id.clone(),
            release: request.release.trim().into(),
            arch: current_arch(),
            components: planned.components.clone(),
            control: clean(
                node.into(),
                control_host.clone(),
                format!(
                    "{why}; the farm is installed clean with OpenStack {}",
                    crate::kolla::OPENSTACK_RELEASE
                ),
            ),
            computes: (1..=planned.computes)
                .map(|index| {
                    clean(
                        format!("compute-{index}"),
                        request
                            .host_of(index)
                            .unwrap_or_else(|| control_host.clone()),
                        "installed with the control VM, which is installed clean".into(),
                    )
                })
                .collect(),
            notes: std::mem::take(notes),
        })
    }

    /// A create request carrying the farm's ID, host and metadata, for either path.
    fn clean_request(&self, request: &ReleaseCreateRequest) -> CreateRequest {
        let host = request.host_id.clone();
        let placement = (1..=request.computes)
            .filter_map(|index| {
                let placed = request.host_of(index)?;
                (Some(&placed) != host.as_ref()).then(|| (format!("compute-{index}"), placed))
            })
            .collect();
        CreateRequest {
            id: request.id.clone(),
            host_id: host,
            components: Vec::new(),
            computes: 0,
            compute: None,
            external_host: request.external_host.clone(),
            owner: request.owner.clone(),
            expires_at: request.expires_at,
            snapshot_id: None,
            placement,
        }
    }

    /// Start the farm the plan of `request` describes, as one assemble job whose steps run
    /// as jobs of their own: imports, the control VM, then each compute in order.
    pub async fn create_release(&self, request: ReleaseCreateRequest) -> Result<Job> {
        let plan = self.preview_release(&request).await?;
        let steps = steps(&plan);
        let job = self.update(|state| {
            self.ensure_accepting()?;
            let id = &request.id;
            if state
                .farms
                .get(id)
                .is_some_and(|f| f.status != FarmStatus::Deleted)
                || state
                    .jobs
                    .values()
                    .any(|j| j.kind == JobKind::Assemble && &j.farm_id == id && j.status.active())
            {
                return Err(conflict(format!("farm '{id}' already exists")));
            }
            let mut job = new_job(id, JobKind::Assemble);
            job.steps = steps;
            state.jobs.insert(job.id.clone(), job.clone());
            Ok(job)
        })?;
        for line in describe(&plan) {
            self.log(&job.farm_id, &job.id, None, format!("{line}\n"))?;
        }
        let manager = self.clone();
        let spawned = job.clone();
        tokio::spawn(async move {
            use futures::FutureExt;
            let result = std::panic::AssertUnwindSafe(manager.assemble(&spawned, &request, &plan))
                .catch_unwind()
                .await
                .unwrap_or_else(|_| Err(anyhow!("job panicked")));
            if let Err(error) = manager.finish_assembly(&spawned, result) {
                eprintln!("cannot persist completion of {}: {error:#}", spawned.id);
            }
        });
        Ok(job)
    }

    async fn assemble(
        &self,
        job: &Job,
        request: &ReleaseCreateRequest,
        plan: &ReleasePlan,
    ) -> Result<()> {
        self.update(|state| {
            let j = state.jobs.get_mut(&job.id).context("job missing")?;
            j.status = JobStatus::Running;
            j.started_at.get_or_insert_with(now);
            Ok(())
        })?;
        let nodes: Vec<&PlannedNode> = std::iter::once(&plan.control)
            .chain(&plan.computes)
            .collect();
        for (index, step) in job.steps.iter().enumerate() {
            self.set_step(&job.id, index, |s| s.status = JobStatus::Running)?;
            self.log(
                &job.farm_id,
                &job.id,
                None,
                format!(
                    "step {}/{}: {}\n",
                    index + 1,
                    job.steps.len(),
                    step.description
                ),
            )?;
            let result = self.run_step(job, index, step, request, plan, &nodes).await;
            let error = result.as_ref().err().map(|e| format!("{e:#}"));
            self.set_step(&job.id, index, |s| {
                s.status = match error {
                    Some(_) => JobStatus::Failed,
                    None => JobStatus::Succeeded,
                };
                s.error = error.clone();
            })?;
            if let Err(error) = result {
                let farm = self.get(&job.farm_id).ok();
                let hint = match (step.action.as_str(), farm) {
                    (_, None) => "nothing was created; start it again".to_owned(),
                    ("expand", Some(_)) => format!(
                        "the farm keeps its other VMs; add {} again{} to retry, or delete the farm",
                        step.node.as_deref().unwrap_or("the compute"),
                        step.snapshot_id
                            .as_ref()
                            .map(|s| format!(" from compute template '{s}'"))
                            .unwrap_or_else(|| " as a clean install".into())
                    ),
                    ("deploy", Some(_)) => "deploy the farm again, or delete it".into(),
                    (_, Some(_)) => "delete the farm and start it again".into(),
                };
                for skipped in index + 1..job.steps.len() {
                    self.set_step(&job.id, skipped, |s| s.status = JobStatus::Interrupted)?;
                }
                bail!("{}: {error:#}; {hint}", step.description);
            }
        }
        Ok(())
    }

    async fn run_step(
        &self,
        job: &Job,
        index: usize,
        step: &AssemblyStep,
        request: &ReleaseCreateRequest,
        plan: &ReleasePlan,
        nodes: &[&PlannedNode],
    ) -> Result<()> {
        let farm = &job.farm_id;
        let child = match step.action.as_str() {
            "import" => {
                let id = step.snapshot_id.clone().context("import has no snapshot")?;
                let node = nodes
                    .iter()
                    .find(|n| n.source == NodeSource::Bucket && n.snapshot_id.as_ref() == Some(&id))
                    .context("import has no bucket")?;
                return self.import_template(&id, node).await;
            }
            "restore" => {
                let snapshot = step
                    .snapshot_id
                    .clone()
                    .context("restore has no snapshot")?;
                let host = step.host_id.clone().context("restore has no host")?;
                self.ensure_template_on_host(&snapshot, &host).await?;
                // Its computes are added one at a time afterwards.
                self.create(CreateRequest {
                    snapshot_id: Some(snapshot),
                    placement: Default::default(),
                    ..self.clean_request(request)
                })?
            }
            "create" => self.create(CreateRequest {
                components: plan.components.clone(),
                computes: plan.computes.len() as u8,
                ..self.clean_request(request)
            })?,
            "deploy" => self.deploy(farm, DeployRequest::default())?,
            "expand" => {
                if let (Some(snapshot), Some(host)) = (&step.snapshot_id, &step.host_id) {
                    self.ensure_template_on_host(snapshot, host).await?;
                }
                self.add_compute(
                    farm,
                    AddComputeRequest {
                        host_id: step.host_id.clone(),
                        from_snapshot: step.snapshot_id.clone(),
                    },
                )?
            }
            action => bail!("unknown step {action}"),
        };
        self.set_step(&job.id, index, |s| s.job_id = Some(child.id.clone()))?;
        let finished = self.wait(&child.id).await?;
        if finished.status != JobStatus::Succeeded {
            bail!(
                "{}",
                finished
                    .error
                    .unwrap_or_else(|| format!("job {} {:?}", finished.id, finished.status))
            );
        }
        Ok(())
    }

    /// Import a bucket template, or wait for the import of it the plan found running,
    /// which may have finished since.
    async fn import_template(&self, id: &str, node: &PlannedNode) -> Result<()> {
        if self.snapshot(id).is_err() {
            self.import_snapshot(ImportRequest {
                path: None,
                bucket: node.bucket.clone(),
                key: node.key.clone(),
                id: Some(id.into()),
            })?;
        }
        let snapshot = self.wait_snapshot(id).await?;
        if snapshot.status != SnapshotStatus::Ready {
            bail!(
                "{}",
                snapshot
                    .error
                    .unwrap_or_else(|| format!("snapshot '{id}' is {:?}", snapshot.status))
            );
        }
        Ok(())
    }

    /// Make `snapshot` available to boot a VM on `host`. Templates are kept on the local
    /// host and the plan only boots them there, so this checks that it still holds.
    async fn ensure_template_on_host(&self, snapshot: &str, host: &str) -> Result<()> {
        self.snapshot(snapshot)?;
        self.read(|state| template_on_host(state, host))?
            .map_err(|why| conflict(format!("template '{snapshot}': {why}")))
    }

    fn set_step(&self, job: &str, index: usize, f: impl FnOnce(&mut AssemblyStep)) -> Result<()> {
        self.update(|state| {
            let step = state
                .jobs
                .get_mut(job)
                .and_then(|job| job.steps.get_mut(index))
                .context("assembly step missing")?;
            f(step);
            Ok(())
        })
    }

    fn finish_assembly(&self, job: &Job, result: Result<()>) -> Result<()> {
        let error = result.err().map(|e| format!("{e:#}"));
        let logged = self.log(
            &job.farm_id,
            &job.id,
            None,
            error.clone().unwrap_or_else(|| "job succeeded".into()),
        );
        self.update(|state| {
            let current = state.jobs.get_mut(&job.id).context("job missing")?;
            current.status = match error {
                Some(_) => JobStatus::Failed,
                None => JobStatus::Succeeded,
            };
            current.error = error.clone();
            current.finished_at = Some(now());
            Ok(())
        })?;
        logged
    }
}

fn clean(node: String, host_id: String, reason: String) -> PlannedNode {
    PlannedNode {
        node,
        host_id,
        source: NodeSource::Clean,
        snapshot_id: None,
        bucket: None,
        key: None,
        reason,
    }
}

fn newest(count: usize, role: &str, release: &str, place: &str) -> String {
    match count {
        1 => format!("the {role} template of OpenStack {release} {place}"),
        n => format!("the newest of {n} {role} templates of OpenStack {release} {place}"),
    }
}

fn mismatch(other: usize, role: &str) -> String {
    match other {
        0 => String::new(),
        n => format!(" ({n} {role} template(s) of it run other services)"),
    }
}

/// The steps that carry out `plan`.
fn steps(plan: &ReleasePlan) -> Vec<AssemblyStep> {
    let step = |action: &str, node: Option<&PlannedNode>, description: String| AssemblyStep {
        action: action.into(),
        node: node.map(|n| n.node.clone()),
        snapshot_id: node.and_then(|n| n.snapshot_id.clone()),
        host_id: node.map(|n| n.host_id.clone()),
        description,
        status: JobStatus::Queued,
        job_id: None,
        error: None,
    };
    let mut steps = Vec::new();
    let mut imported = BTreeSet::new();
    for node in std::iter::once(&plan.control).chain(&plan.computes) {
        if let (NodeSource::Bucket, Some(id), Some(bucket), Some(key)) =
            (node.source, &node.snapshot_id, &node.bucket, &node.key)
            && imported.insert(id.clone())
        {
            let mut import = step(
                "import",
                None,
                format!("import {key} from bucket '{bucket}' as snapshot '{id}'"),
            );
            import.snapshot_id = Some(id.clone());
            steps.push(import);
        }
    }
    let on = |node: &PlannedNode| format!("{} on host '{}'", node.node, node.host_id);
    match plan.control.source {
        NodeSource::Clean => {
            let computes = plan.computes.len();
            steps.push(step(
                "create",
                Some(&plan.control),
                match computes {
                    0 => format!("create {} clean", on(&plan.control)),
                    n => format!("create {} and {n} compute(s) clean", on(&plan.control)),
                },
            ));
            let mut deploy = step(
                "deploy",
                None,
                format!("deploy OpenStack {} with Kolla", plan.release),
            );
            deploy.node = None;
            steps.push(deploy);
        }
        NodeSource::Template | NodeSource::Bucket => {
            steps.push(step(
                "restore",
                Some(&plan.control),
                format!(
                    "start {} from control template '{}'",
                    on(&plan.control),
                    plan.control.snapshot_id.as_deref().unwrap_or_default()
                ),
            ));
            for compute in &plan.computes {
                steps.push(step(
                    "expand",
                    Some(compute),
                    match &compute.snapshot_id {
                        Some(snapshot) => {
                            format!("add {} from compute template '{snapshot}'", on(compute))
                        }
                        None => format!("add {} as a clean install", on(compute)),
                    },
                ));
            }
        }
    }
    steps
}

/// The plan as log lines.
pub fn describe(plan: &ReleasePlan) -> Vec<String> {
    let line = |node: &PlannedNode| {
        let source = match (node.source, &node.snapshot_id) {
            (NodeSource::Clean, _) => "clean install".to_owned(),
            (NodeSource::Template, Some(id)) => format!("template {id}"),
            (NodeSource::Bucket, Some(id)) => format!(
                "template {id} imported from {} in bucket {}",
                node.key.as_deref().unwrap_or_default(),
                node.bucket.as_deref().unwrap_or_default()
            ),
            (_, None) => "template".into(),
        };
        format!(
            "{} on {}: {source} — {}",
            node.node, node.host_id, node.reason
        )
    };
    let mut lines = vec![format!(
        "OpenStack {} on {} with {}",
        plan.release,
        plan.arch,
        plan.components.join(",")
    )];
    lines.extend(
        std::iter::once(&plan.control)
            .chain(&plan.computes)
            .map(line),
    );
    lines.extend(plan.notes.iter().map(|note| format!("note: {note}")));
    lines
}
