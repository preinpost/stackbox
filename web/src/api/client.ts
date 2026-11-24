import {
  queryOptions,
  useMutation,
  useQueryClient,
} from "@tanstack/react-query";
import type { components } from "./schema";

type Schema = components["schemas"];
export type Stackbox = Schema["StackboxView"];
export type Host = Schema["HostInfo"];
export type RelayStatus = Schema["RelayStatus"];
export type RelaySettings = Schema["RelaySettings"];
export type Catalog = Schema["Catalog"];
export type Plan = Schema["Plan"];
export type ComputeSize = Schema["ComputeSize"];
export type GpuConfig = Schema["GpuConfig"];
export type GpuMode = Schema["GpuMode"];
export type GpuLoad = Schema["GpuLoad"];
export type GpuLoadView = Schema["GpuLoadView"];
export type GpuPattern = Schema["GpuPattern"];
export type GpuProfile = Schema["GpuProfile"];
export type StackboxInstance = Schema["StackboxInstance"];
export type StackboxLayout = Schema["StackboxLayout"];
export type Phase = Schema["Phase"];
export type Job = Schema["Job"];
export type LogEntry = Schema["LogEntry"];
export type CreateRequest = Schema["CreateRequest"];
export type ServiceEndpoints = Pick<
  Schema["Endpoints"],
  "services" | "credentials" | "datastores"
>;
export type Datastore = Schema["Datastore"];
export type RemoteHost = Schema["RemoteHost"];
export type HostInspection = Schema["HostInspection"];
export type HostPciDevice = Schema["HostPciDevice"];
export type HostPciCandidate = Schema["HostPciCandidate"];
export type HostPciHelperInstall = Schema["PciHelperInstall"];
export type HostCapacity = Schema["HostCapacity"];
export type HostJob = Schema["HostJob"];
export type HostKey = Schema["HostKey"];
export type RegisterHost = Schema["RegisterHost"];
export type AgentHostToken = Schema["AgentHostToken"];
export type Snapshot = Schema["SnapshotRecord"];
export type SnapshotRequest = Schema["SnapshotRequest"];
export type ImportRequest = Schema["ImportRequest"];
export type ExportRequest = Schema["ExportRequest"];
export type Bucket = Schema["Bucket"];
export type Impair = Schema["Impair"];
export type RegisterBucket = Schema["RegisterBucket"];
export type BucketObject = Schema["BucketObject"];
export type ArchiveMetadata = Schema["ArchiveMetadata"];
export type ReleaseCatalog = Schema["ReleaseCatalog"];
export type ReleaseTemplates = Schema["ReleaseTemplates"];
export type ReleaseCreateRequest = Schema["ReleaseCreateRequest"];
export type ReleasePlan = Schema["ReleasePlan"];
export type PlannedNode = Schema["PlannedNode"];
export type AssemblyStep = Schema["AssemblyStep"];

export class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
  ) {
    super(message);
  }
}
export async function request<T>(
  path: string,
  options: RequestInit = {},
): Promise<T> {
  const response = await fetch(`/api/v1${path}`, {
    ...options,
    headers: {
      ...(options.body ? { "Content-Type": "application/json" } : {}),
      ...options.headers,
    },
    cache: "no-store",
  });
  const payload = await response.text();
  let data: unknown;
  try {
    data = JSON.parse(payload);
  } catch {
    data = undefined;
  }
  if (!response.ok) {
    const message =
      data && typeof data === "object" && "error" in data
        ? String(data.error)
        : payload || "요청에 실패했습니다.";
    throw new ApiError(response.status, message);
  }
  if (data === undefined)
    throw new ApiError(502, "서버 응답을 읽을 수 없습니다.");
  return data as T;
}
const encode = encodeURIComponent;
export const activeJob = (job: Pick<Job, "status">) =>
  job.status === "queued" || job.status === "running";
export const busyStackbox = (stackbox: Stackbox) => Boolean(stackbox.active_job);
export const busySnapshot = (snapshot: Pick<Snapshot, "status">) =>
  ["creating", "exporting", "importing", "deleting"].includes(snapshot.status);
export const keys = {
  stackboxes: ["stackboxes"] as const,
  jobs: ["jobs"] as const,
  host: ["host"] as const,
  hosts: ["remote-hosts"] as const,
  hostJobs: ["host-jobs"] as const,
  relay: ["relay"] as const,
  snapshots: ["snapshots"] as const,
  buckets: ["buckets"] as const,
  bucketObjects: (id: string) => ["buckets", id, "objects"] as const,
  releases: ["releases"] as const,
  stackbox: (id: string) => ["stackboxes", id] as const,
  logs: (id: string) => ["logs", id] as const,
  endpoints: (id: string) => ["endpoints", id] as const,
  gpuInstances: (id: string) => ["gpu-instances", id] as const,
};
export const stackboxesQuery = () =>
  queryOptions({
    queryKey: keys.stackboxes,
    queryFn: ({ signal }) => request<Stackbox[]>("/stackboxes", { signal }),
    refetchInterval: (q) => (q.state.data?.some(busyStackbox) ? 2000 : 10_000),
  });
export const jobsQuery = () =>
  queryOptions({
    queryKey: keys.jobs,
    queryFn: ({ signal }) => request<Job[]>("/jobs", { signal }),
    refetchInterval: (q) => (q.state.data?.some(activeJob) ? 1500 : 10_000),
  });
export const hostQuery = () =>
  queryOptions({
    queryKey: keys.host,
    queryFn: ({ signal }) => request<Host>("/host", { signal }),
    refetchInterval: 15_000,
    staleTime: 10_000,
  });
export const relayQuery = () =>
  queryOptions({
    queryKey: keys.relay,
    queryFn: ({ signal }) => request<RelayStatus>("/relay", { signal }),
    // Follow a tunnel closely while cloudflared downloads, connects and gets its address.
    refetchInterval: (q) =>
      tunnelStage(q.state.data) === "ready" ? 15_000 : 1000,
  });
export const hostsQuery = () =>
  queryOptions({
    queryKey: keys.hosts,
    queryFn: ({ signal }) => request<RemoteHost[]>("/hosts", { signal }),
    refetchInterval: (q) =>
      q.state.data?.some((h) => h.active_job) ? 1500 : 15_000,
  });
export const hostJobsQuery = () =>
  queryOptions({
    queryKey: keys.hostJobs,
    queryFn: ({ signal }) => request<HostJob[]>("/host-jobs", { signal }),
    refetchInterval: (q) =>
      q.state.data?.some((j) => ["queued", "running"].includes(j.status))
        ? 1500
        : 10_000,
  });
export const snapshotsQuery = () =>
  queryOptions({
    queryKey: keys.snapshots,
    queryFn: ({ signal }) => request<Snapshot[]>("/snapshots", { signal }),
    refetchInterval: (q) => (q.state.data?.some(busySnapshot) ? 1500 : 10_000),
  });
export const bucketsQuery = () =>
  queryOptions({
    queryKey: keys.buckets,
    queryFn: ({ signal }) => request<Bucket[]>("/buckets", { signal }),
  });
/** Archives in a bucket, read from the object store on each open. */
export const bucketObjectsQuery = (id: string) =>
  queryOptions({
    queryKey: keys.bucketObjects(id),
    queryFn: ({ signal }) =>
      request<BucketObject[]>(`/buckets/${encode(id)}/objects`, { signal }),
    staleTime: 0,
    retry: false,
  });
/** Releases with the templates kept here and in every bucket, on `arch` (this host's by default). */
export const releasesQuery = (arch?: string) =>
  queryOptions({
    queryKey: [...keys.releases, arch ?? null],
    queryFn: ({ signal }) =>
      request<ReleaseCatalog>(
        arch ? `/releases?arch=${encode(arch)}` : "/releases",
        { signal },
      ),
    staleTime: 30_000,
  });
/** The stackbox's Nova servers and their GPU loads; refreshed while a deploy changes them. */
export const gpuInstancesQuery = (id: string) =>
  queryOptions({
    queryKey: keys.gpuInstances(id),
    queryFn: ({ signal }) =>
      request<StackboxInstance[]>(`/stackboxes/${encode(id)}/instances`, { signal }),
    refetchInterval: 15_000,
  });
/** With a release, each service says whether that release can run it. */
export const catalogQuery = (release?: string) =>
  queryOptions({
    queryKey: ["catalog", release ?? null],
    queryFn: ({ signal }) =>
      request<Catalog>(
        release ? `/catalog?release=${encode(release)}` : "/catalog",
        { signal },
      ),
    staleTime: Infinity,
  });
export const stackboxQuery = (id: string) =>
  queryOptions({
    queryKey: keys.stackbox(id),
    queryFn: ({ signal }) => request<Stackbox>(`/stackboxes/${encode(id)}`, { signal }),
    refetchInterval: (q) => (q.state.data?.active_job ? 1500 : 10_000),
  });
export const logsQuery = (id: string) =>
  queryOptions({
    queryKey: keys.logs(id),
    queryFn: ({ signal }) =>
      request<LogEntry[]>(`/stackboxes/${encode(id)}/logs`, { signal }),
    staleTime: Infinity,
  });
/** Binds a host PCI device to `vfio-pci` through the host's helper; the job reports the result. */
export const bindHostPci = (hostId: string, bdf: string) =>
  request<HostJob>(`/hosts/${encode(hostId)}/pci/bind`, {
    method: "POST",
    body: JSON.stringify({ bdf }),
  });
/** Gives a device back to its host driver through the helper. */
export const unbindHostPci = (hostId: string, bdf: string) =>
  request<HostJob>(`/hosts/${encode(hostId)}/pci/unbind`, {
    method: "POST",
    body: JSON.stringify({ bdf }),
  });
/** The helper's install script. Non-SSH hosts must name the login user the script is for. */
export const fetchHostPciHelperInstall = (hostId: string, user?: string) =>
  request<HostPciHelperInstall>(
    `/hosts/${encode(hostId)}/pci/helper-install${user ? `?user=${encode(user)}` : ""}`,
  );

function useHostPciMutation(
  run: (hostId: string, bdf: string) => Promise<HostJob>,
) {
  const client = useQueryClient();
  return useMutation({
    mutationFn: ({ hostId, bdf }: { hostId: string; bdf: string }) =>
      run(hostId, bdf),
    onSettled: () =>
      Promise.all([
        client.invalidateQueries({ queryKey: keys.hosts }),
        client.invalidateQueries({ queryKey: keys.hostJobs }),
      ]),
  });
}
export const useBindHostPci = () => useHostPciMutation(bindHostPci);
export const useUnbindHostPci = () => useHostPciMutation(unbindHostPci);

export const api = {
  updateHostCredentials: (id: string, body: RegisterHost) =>
    request<RemoteHost>(`/hosts/${encode(id)}`, {
      method: "PUT",
      body: JSON.stringify(body),
    }),
  /** Deprecated endpoint: preparation installs the agent too. Kept for older callers. */
  installHostAgent: (id: string) =>
    request<HostJob>(`/hosts/${encode(id)}/agent`, { method: "POST" }),
  fingerprint: (address: string, port: number) =>
    request<HostKey>("/hosts/fingerprint", {
      method: "POST",
      body: JSON.stringify({ address, port }),
    }),
  testConnection: (body: RegisterHost) =>
    request<{ identity: string }>("/hosts/test-connection", {
      method: "POST",
      body: JSON.stringify({
        address: body.address,
        port: body.port,
        username: body.username,
        fingerprint: body.fingerprint,
        credentials: body.credentials,
      }),
    }),
  registerHost: (body: RegisterHost) =>
    request<RemoteHost>("/hosts", {
      method: "POST",
      body: JSON.stringify(body),
    }),
  /** Register the machine the coordinator runs on; its agent is the coordinator itself. */
  registerLocalHost: (id = "local") =>
    request<RemoteHost>("/local-host", {
      method: "POST",
      body: JSON.stringify({ id }),
    }),
  /** Register a host whose agent dials in; the token is returned only here. */
  registerAgentHost: (id: string) =>
    request<AgentHostToken>("/agent-hosts", {
      method: "POST",
      body: JSON.stringify({ id }),
    }),
  checkHost: (id: string) =>
    request<HostJob>(`/hosts/${encode(id)}/check`, { method: "POST" }),
  prepareHost: (id: string) =>
    request<HostJob>(`/hosts/${encode(id)}/prepare`, { method: "POST" }),
  unregisterHost: (id: string) =>
    request<{ id: string }>(`/hosts/${encode(id)}`, { method: "DELETE" }),
  /** Delete this coordinator's stackboxes and leftover VMs on the host, then unregister it.
   * `snapshots` also deletes the snapshots whose original the host keeps. */
  purgeHost: (id: string, snapshots = false) =>
    request<HostJob>(
      `/hosts/${encode(id)}?purge=true${snapshots ? "&snapshots=true" : ""}`,
      { method: "DELETE" },
    ),
  /** Where stackboxes that span hosts join; `null` turns it off. */
  setRelay: (body: RelaySettings | null) =>
    request<RelayStatus>("/relay", {
      method: body ? "PUT" : "DELETE",
      ...(body ? { body: JSON.stringify(body) } : {}),
    }),
  updateCloudflared: () =>
    request<RelayStatus>("/relay/cloudflared/update", { method: "POST" }),
  create: (body: CreateRequest) =>
    request<Job>("/stackboxes", { method: "POST", body: JSON.stringify(body) }),
  /** Dry run: which template or clean install makes each VM, and why. */
  releasePlan: (body: ReleaseCreateRequest, signal?: AbortSignal) =>
    request<ReleasePlan>("/releases/plan", {
      method: "POST",
      body: JSON.stringify(body),
      signal,
    }),
  /** Starts an assemble job; its steps are jobs of their own. */
  createRelease: (body: ReleaseCreateRequest) =>
    request<Job>("/releases/stackboxes", {
      method: "POST",
      body: JSON.stringify(body),
    }),
  deploy: (id: string, from: Phase) =>
    request<Job>(`/stackboxes/${encode(id)}/deploy`, {
      method: "POST",
      body: JSON.stringify({ from }),
    }),
  /** Add the next compute VM; a host other than the stackbox's primary joins through the relay.
   * `fromSnapshot` boots it from a compute template instead of installing it clean. */
  addCompute: (id: string, hostId?: string, fromSnapshot?: string) =>
    request<Job>(`/stackboxes/${encode(id)}/computes`, {
      method: "POST",
      body: JSON.stringify({
        host_id: hostId ?? null,
        ...(fromSnapshot ? { from_snapshot: fromSnapshot } : {}),
      }),
    }),
  /** `forget` removes only the record, without asking the stackbox's host. */
  delete: (id: string, forget = false) =>
    request<Job>(`/stackboxes/${encode(id)}${forget ? "?forget=true" : ""}`, {
      method: "DELETE",
    }),
  plan: (
    components: string[],
    computes = 1,
    signal?: AbortSignal,
    compute?: ComputeSize,
    gpu?: GpuConfig,
    pciDevices?: string[],
  ) =>
    request<Plan>("/plans", {
      method: "POST",
      body: JSON.stringify({
        components,
        computes,
        compute,
        ...(gpu ? { gpu } : {}),
        ...(pciDevices?.length ? { pci_devices: pciDevices } : {}),
      }),
      signal,
    }),
  /** No delay and no loss heals the VM. */
  setFaults: (stackboxId: string, node: string, impair: Impair) =>
    request<Stackbox>(`/stackboxes/${encode(stackboxId)}/nodes/${encode(node)}/faults`, {
      method: "PUT",
      body: JSON.stringify(impair),
    }),
  endpoints: (id: string) =>
    request<ServiceEndpoints>(`/stackboxes/${encode(id)}/endpoints`),
  instances: (id: string) =>
    request<StackboxInstance[]>(`/stackboxes/${encode(id)}/instances`),
  gpuLoad: (id: string, server: string) =>
    request<GpuLoadView>(
      `/stackboxes/${encode(id)}/instances/${encode(server)}/gpu-load`,
    ),
  /** Writes the sample table to the instance's Nova metadata. */
  setGpuLoad: (id: string, server: string, load: GpuLoad) =>
    request<GpuLoadView>(
      `/stackboxes/${encode(id)}/instances/${encode(server)}/gpu-load`,
      { method: "PUT", body: JSON.stringify(load) },
    ),
  clearGpuLoad: (id: string, server: string) =>
    request<GpuLoadView>(
      `/stackboxes/${encode(id)}/instances/${encode(server)}/gpu-load`,
      { method: "DELETE" },
    ),
  settle: (id: string) =>
    request<Job>(`/stackboxes/${encode(id)}/settle`, { method: "POST" }),
  freeze: (stackboxId: string, body: SnapshotRequest) =>
    request<Job>(`/stackboxes/${encode(stackboxId)}/snapshots`, {
      method: "POST",
      body: JSON.stringify(body),
    }),
  exportSnapshot: (id: string, body: ExportRequest = {}) =>
    request<Snapshot>(`/snapshots/${encode(id)}/export`, {
      method: "POST",
      body: JSON.stringify(body),
    }),
  importSnapshot: (body: ImportRequest) =>
    request<Snapshot>("/snapshots/import", {
      method: "POST",
      body: JSON.stringify(body),
    }),
  /** `forget` removes only the record, without asking the host that keeps the disk.
   * Copies on other hosts are removed too; `copies_left` names those that were not. */
  deleteSnapshot: (id: string, forget = false) =>
    request<Schema["RemovedSnapshot"]>(
      `/snapshots/${encode(id)}${forget ? "?forget=true" : ""}`,
      { method: "DELETE" },
    ),
  /** Remove the copy another host keeps; the snapshot stays. */
  removeSnapshotCopy: (id: string, hostId: string) =>
    request<Snapshot>(`/snapshots/${encode(id)}/copies/${encode(hostId)}`, {
      method: "DELETE",
    }),
  registerBucket: (body: RegisterBucket) =>
    request<Bucket>("/buckets", {
      method: "POST",
      body: JSON.stringify(body),
    }),
  /** Without `secret_access_key`, the stored key is kept. */
  updateBucket: (body: RegisterBucket) =>
    request<Bucket>(`/buckets/${encode(body.id)}`, {
      method: "PUT",
      body: JSON.stringify(body),
    }),
  unregisterBucket: (id: string) =>
    request<{ id: string }>(`/buckets/${encode(id)}`, { method: "DELETE" }),
  /** Try settings before saving them; nothing is stored. */
  testBucket: (body: RegisterBucket) =>
    request<{ message: string }>("/buckets/test-connection", {
      method: "POST",
      body: JSON.stringify(body),
    }),
  checkBucket: (id: string) =>
    request<{ message: string }>(`/buckets/${encode(id)}/check`, {
      method: "POST",
    }),
};

/** Where a tunnel the coordinator runs is: no tunnel at all is "ready". */
export function tunnelStage(
  relay?: RelayStatus,
): "download" | "connect" | "address" | "ready" | "failed" {
  const tunnel = relay?.tunnel;
  if (!tunnel) return "ready";
  if (tunnel.error && !tunnel.connected) return "failed";
  if (!tunnel.version && !tunnel.unmanaged) return "download";
  if (!tunnel.connected) return "connect";
  if (relay?.settings?.mode === "quick_tunnel" && !tunnel.url) return "address";
  return "ready";
}
