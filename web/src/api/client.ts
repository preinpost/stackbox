import { queryOptions } from "@tanstack/react-query";
import type { components } from "./schema";

type Schema = components["schemas"];
export type Farm = Schema["FarmRecord"];
export type Host = Schema["HostInfo"];
export type Catalog = Schema["Catalog"];
export type Plan = Schema["Plan"];
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
export type HostCapacity = Schema["HostCapacity"];
export type HostJob = Schema["HostJob"];
export type HostKey = Schema["HostKey"];
export type RegisterHost = Schema["RegisterHost"];
export type AgentHostToken = Schema["AgentHostToken"];
export type Snapshot = Schema["SnapshotRecord"];
export type SnapshotRequest = Schema["SnapshotRequest"];
export type ImportRequest = Schema["ImportRequest"];

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
export const busyFarm = (farm: Farm) => Boolean(farm.active_job);
export const busySnapshot = (snapshot: Pick<Snapshot, "status">) =>
  ["creating", "exporting", "importing", "deleting"].includes(snapshot.status);
export const keys = {
  farms: ["farms"] as const,
  jobs: ["jobs"] as const,
  host: ["host"] as const,
  hosts: ["remote-hosts"] as const,
  hostJobs: ["host-jobs"] as const,
  snapshots: ["snapshots"] as const,
  farm: (id: string) => ["farms", id] as const,
  logs: (id: string) => ["logs", id] as const,
  endpoints: (id: string) => ["endpoints", id] as const,
};
export const farmsQuery = () =>
  queryOptions({
    queryKey: keys.farms,
    queryFn: ({ signal }) => request<Farm[]>("/farms", { signal }),
    refetchInterval: (q) => (q.state.data?.some(busyFarm) ? 2000 : 10_000),
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
export const catalogQuery = () =>
  queryOptions({
    queryKey: ["catalog"],
    queryFn: ({ signal }) => request<Catalog>("/catalog", { signal }),
    staleTime: Infinity,
  });
export const farmQuery = (id: string) =>
  queryOptions({
    queryKey: keys.farm(id),
    queryFn: ({ signal }) => request<Farm>(`/farms/${encode(id)}`, { signal }),
    refetchInterval: (q) => (q.state.data?.active_job ? 1500 : 10_000),
  });
export const logsQuery = (id: string) =>
  queryOptions({
    queryKey: keys.logs(id),
    queryFn: ({ signal }) =>
      request<LogEntry[]>(`/farms/${encode(id)}/logs`, { signal }),
    staleTime: Infinity,
  });
export const api = {
  updateHostCredentials: (id: string, body: RegisterHost) =>
    request<RemoteHost>(`/hosts/${encode(id)}`, {
      method: "PUT",
      body: JSON.stringify(body),
    }),
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
  /** Delete this coordinator's farms and leftover VMs on the host, then unregister it.
   * `snapshots` also deletes the snapshots, which only the local host keeps. */
  purgeHost: (id: string, snapshots = false) =>
    request<HostJob>(
      `/hosts/${encode(id)}?purge=true${snapshots ? "&snapshots=true" : ""}`,
      { method: "DELETE" },
    ),
  create: (body: CreateRequest) =>
    request<Job>("/farms", { method: "POST", body: JSON.stringify(body) }),
  deploy: (id: string, from: Phase) =>
    request<Job>(`/farms/${encode(id)}/deploy`, {
      method: "POST",
      body: JSON.stringify({ from }),
    }),
  delete: (id: string) =>
    request<Job>(`/farms/${encode(id)}`, { method: "DELETE" }),
  plan: (components: string[], signal?: AbortSignal) =>
    request<Plan>("/plans", {
      method: "POST",
      body: JSON.stringify({ components }),
      signal,
    }),
  endpoints: (id: string) =>
    request<ServiceEndpoints>(`/farms/${encode(id)}/endpoints`),
  settle: (id: string) =>
    request<Job>(`/farms/${encode(id)}/settle`, { method: "POST" }),
  freeze: (farmId: string, body: SnapshotRequest) =>
    request<Job>(`/farms/${encode(farmId)}/snapshots`, {
      method: "POST",
      body: JSON.stringify(body),
    }),
  exportSnapshot: (id: string, path?: string) =>
    request<Snapshot>(`/snapshots/${encode(id)}/export`, {
      method: "POST",
      body: JSON.stringify(path ? { path } : {}),
    }),
  importSnapshot: (body: ImportRequest) =>
    request<Snapshot>("/snapshots/import", {
      method: "POST",
      body: JSON.stringify(body),
    }),
  deleteSnapshot: (id: string) =>
    request<{ id: string }>(`/snapshots/${encode(id)}`, { method: "DELETE" }),
};
