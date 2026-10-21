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
  "services" | "credentials"
>;
export type RemoteHost = Schema["RemoteHost"];
export type HostCapacity = Schema["HostCapacity"];
export type HostJob = Schema["HostJob"];
export type HostKey = Schema["HostKey"];
export type RegisterHost = Schema["RegisterHost"];

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
export const keys = {
  farms: ["farms"] as const,
  jobs: ["jobs"] as const,
  host: ["host"] as const,
  hosts: ["remote-hosts"] as const,
  hostJobs: ["host-jobs"] as const,
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
  checkHost: (id: string) =>
    request<HostJob>(`/hosts/${encode(id)}/check`, { method: "POST" }),
  prepareHost: (id: string) =>
    request<HostJob>(`/hosts/${encode(id)}/prepare`, { method: "POST" }),
  unregisterHost: (id: string) =>
    request<{ id: string }>(`/hosts/${encode(id)}`, { method: "DELETE" }),
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
};
