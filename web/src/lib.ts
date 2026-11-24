import type {
  ArchiveMetadata,
  Stackbox,
  Job,
  Phase,
  RemoteHost,
} from "./api/client";
export const jobKindLabels: Record<Job["kind"], string> = {
  create: "VM 생성",
  deploy: "OpenStack 배포",
  expand: "Compute 추가",
  delete: "Stackbox 삭제",
  snapshot: "스냅샷 얼리기",
  restore: "스냅샷에서 복원",
  settle: "서비스 응답 확인",
  assemble: "릴리스로 만들기",
};
export const phases: { id: Phase; label: string; description: string }[] = [
  {
    id: "prepare",
    label: "Prepare",
    description: "단일 VM과 컨테이너 실행 환경 준비",
  },
  {
    id: "install",
    label: "Install",
    description: "Kolla와 OpenStack CLI 설치",
  },
  {
    id: "configure",
    label: "Configure",
    description: "서비스 설정과 inventory 작성",
  },
  {
    id: "bootstrap",
    label: "Bootstrap",
    description: "컨테이너 호스트 초기화",
  },
  { id: "prechecks", label: "Prechecks", description: "배포 전 사전 검사" },
  { id: "deploy", label: "Deploy", description: "OpenStack 서비스 배포" },
  {
    id: "post-deploy",
    label: "Post-deploy",
    description: "인증정보와 마무리 설정",
  },
  { id: "verify", label: "Verify", description: "Token과 이미지 API 검증" },
];
/** The msb release this coordinator installs on hosts and expects them to run. */
export const MSB_REQUIRED_VERSION = "0.8.1";

export const memory = (mib: number) => `${Number((mib / 1024).toFixed(1))} GiB`;
/** Where a snapshot can be restored: its guest boots only on the same architecture. */
/** A host agent built from other sources cannot parse this coordinator's requests. */
export const agentBuildMismatch = (
  artifact: { build_id?: string | null } | undefined,
  coordinatorBuildId?: string | null,
) =>
  Boolean(
    artifact?.build_id &&
    coordinatorBuildId &&
    artifact.build_id !== coordinatorBuildId,
  );
/** The host that keeps a snapshot's original: the remote host it was frozen on, or the
 * local host, which keeps imported snapshots and older ones. */
export const snapshotOrigin = (
  snapshot: { host_id?: string | null },
  hosts: { id: string; kind?: string }[] = [],
) => snapshot.host_id ?? hosts.find((h) => h.kind === "local")?.id;
export const snapshotHost = (snapshot: {
  arch?: string | null;
  platform?: string | null;
}) =>
  snapshot.platform
    ? `${snapshot.arch ?? "unknown"} · ${platformName(snapshot.platform)}`
    : (snapshot.arch ?? "unknown");
export const platformName = (os: string) =>
  ({ macos: "macOS", linux: "Linux", windows: "Windows" })[os] ?? os;
export const bytes = (value: number) =>
  value >= 1024 ** 3
    ? `${Number((value / 1024 ** 3).toFixed(1))} GiB`
    : `${Number((value / 1024 ** 2).toFixed(1))} MiB`;
export const transferRate = (bytesPerSecond: number) =>
  bytesPerSecond >= 1024 ** 3
    ? `${Number((bytesPerSecond / 1024 ** 3).toFixed(1))} GiB/s`
    : bytesPerSecond >= 1024 ** 2
      ? `${Number((bytesPerSecond / 1024 ** 2).toFixed(1))} MiB/s`
      : `${Math.max(1, Math.round(bytesPerSecond / 1024))} KiB/s`;
const osLabels: Record<string, string> = { macos: "macOS", linux: "Linux" };
export const platform = (os: string, arch: string) =>
  `${osLabels[os] ?? os} / ${arch}`;
export const stackboxMemory = (stackbox: Stackbox) =>
  stackbox.plan?.memory_mib ?? stackbox.nodes.reduce((sum, n) => sum + n.memory_mib, 0);
/** Recorded roles are preserved for legacy stackboxes; only new all-in-one stackboxes support deployment. */
export const isAllInOneStackbox = (stackbox: Stackbox) => {
  const roles = stackbox.plan?.nodes ?? stackbox.nodes;
  return roles.length === 1 && String(roles[0].role) === "all-in-one";
};
export const stackboxNodeCount = (stackbox: Stackbox) =>
  stackbox.plan?.nodes.length ?? stackbox.nodes.length;
/** What a stackbox is doing now. A coordinator that predates `snapshotting` keeps a stackbox ready
 * while its snapshot job stops the VMs, so the job names the state the badge should show. */
export const stackboxState = (stackbox: Stackbox, job?: Job) =>
  ["ready", "created"].includes(stackbox.status) && job?.kind === "snapshot"
    ? "snapshotting"
    : stackbox.status;
/** A stackbox whose deployment still stands: ready, or holding still while its VM is captured.
 * Its services cannot answer during a snapshot, so only labels use this, not requests. */
export const stackboxDeployed = (stackbox: Stackbox) =>
  ["ready", "snapshotting"].includes(stackbox.status);
/** `2 / 2 running` for a stackbox's VMs. A compute being added is already observed on its host
 * before the plan keeps it, so the count says which one that is. */
export const runningVms = (stackbox: Stackbox, job?: Job) => {
  const running = stackbox.nodes.filter((node) => node.status === "running").length;
  const adding = pendingCompute(stackbox, job) ? " · 1대 추가 중" : "";
  return `${running} / ${stackboxNodeCount(stackbox)} running${adding}`;
};
/** Memory of a compute VM made without a chosen size, as the API plans it. */
const DEFAULT_COMPUTE_MIB = 3072;
/** The compute VM an expansion job is adding. Its plan is committed only once Kolla accepts
 * it, so until then the stackbox still shows the computes it had; this names the one on its way. */
export const pendingCompute = (stackbox: Stackbox, job?: Job) => {
  if (job?.kind !== "expand" || !job.target_node) return undefined;
  const index = Number(job.target_node.slice("compute-".length));
  if (!Number.isInteger(index) || index < 1) return undefined;
  if ((stackbox.plan?.computes ?? 0) >= index) return undefined;
  return {
    role: "compute" as const,
    index,
    host: job.target_host ?? stackbox.host_id ?? "",
    memory_mib: stackbox.plan?.compute?.memory_mib ?? DEFAULT_COMPUTE_MIB,
  };
};
/** A control VM and compute VMs joined by the host's link switch. A stackbox made from a
 * control template has its control VM alone until a compute is added. */
export const isMultiNodeStackbox = (stackbox: Stackbox) =>
  String(stackbox.plan?.nodes[0]?.role) === "control";
/** A multi-node stackbox made from a control template that has no compute VM yet. */
export const awaitsCompute = (stackbox: Stackbox) =>
  isMultiNodeStackbox(stackbox) && (stackbox.plan?.computes ?? 0) === 0;
/** All-in-one and multi-node stackboxes deploy; legacy multi-VM layouts do not. */
export const isDeployableStackbox = (stackbox: Stackbox) =>
  isAllInOneStackbox(stackbox) || isMultiNodeStackbox(stackbox);
/** Every planned VM exists and runs. */
export const stackboxNodesRunning = (stackbox: Stackbox) =>
  stackbox.nodes.length === stackboxNodeCount(stackbox) &&
  stackbox.nodes.every((n) => n.status === "running");
type NodeRef = { role?: string | null; index?: number | null };
/** A multi-node VM's name after `<stackbox>-`, and its fault target: `control`, `compute-2`. */
export const nodeSuffix = ({ role, index }: NodeRef) =>
  index ? `${role}-${index}` : String(role);
/** The fixed hostname and management address every stackbox gives this VM. */
export const nodeHostname = (node: NodeRef) =>
  String(node.role) === "all-in-one" ? "stackbox-aio" : `stackbox-${nodeSuffix(node)}`;
export const nodeAddress = ({ role, index }: NodeRef) =>
  String(role) === "compute" && index
    ? `10.254.0.${10 + index}`
    : String(role) === "control"
      ? "10.254.0.10"
      : "10.254.0.2";
/** A span of seconds, e.g. `42초`, `3분 5초`, `1시간 2분`. */
export function duration(span: number) {
  const seconds = Math.max(0, Math.floor(span));
  if (seconds < 60) return `${seconds}초`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}분 ${seconds % 60}초`;
  return `${Math.floor(seconds / 3600)}시간 ${Math.floor((seconds % 3600) / 60)}분`;
}
export const dateTime = (timestamp: number) =>
  new Intl.DateTimeFormat("ko-KR", {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  }).format(timestamp * 1000);
export function relativeTime(timestamp: number) {
  const seconds = Math.max(0, Math.floor(Date.now() / 1000 - timestamp));
  if (seconds < 60) return "방금 전";
  if (seconds < 3600) return `${Math.floor(seconds / 60)}분 전`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}시간 전`;
  return `${Math.floor(seconds / 86400)}일 전`;
}
export const componentInfo: Record<
  string,
  { label: string; description: string }
> = {
  keystone: {
    label: "Keystone",
    description: "Identity · 인증과 서비스 카탈로그",
  },
  glance: { label: "Glance", description: "Image · VM 이미지 저장소" },
  placement: { label: "Placement", description: "자원 인벤토리와 할당 추적" },
  horizon: { label: "Horizon", description: "웹 대시보드" },
  neutron: {
    label: "Neutron",
    description: "Network · 가상 네트워크와 라우터",
  },
  nova: {
    label: "Nova",
    description: "Compute · KVM이 없으면 TCG로 작은 인스턴스",
  },
  heat: {
    label: "Heat",
    description: "Orchestration · 템플릿으로 리소스 스택 생성",
  },
  barbican: {
    label: "Barbican",
    description: "Key Manager · 비밀·인증서 저장",
  },
  designate: {
    label: "Designate",
    description: "DNS · 존과 레코드 관리",
  },
  magnum: {
    label: "Magnum",
    description: "Container Infra · Kubernetes 클러스터 생성",
  },
  skyline: {
    label: "Skyline",
    description: "Dashboard · 새 웹 콘솔",
  },
};
export const componentLabel = (id: string) => componentInfo[id]?.label ?? id;
/** What a bucket archive holds, from the metadata file its export wrote next to it.
 * Archives exported before those files existed say so and can still be imported. */
export function archiveSummary(metadata?: ArchiveMetadata | null) {
  if (!metadata) return { title: "메타데이터 없음", detail: "" };
  const role = metadata.role
    ? `${metadata.role === "control" ? "Control" : "Compute"} 템플릿`
    : (metadata.computes ?? 0) > 0
      ? `멀티노드 stackbox 스냅샷 (compute ${metadata.computes}대)`
      : "stackbox 스냅샷";
  return {
    title: [
      metadata.release ? `OpenStack ${metadata.release}` : "릴리스 미상",
      role,
    ].join(" · "),
    detail: [
      metadata.components.map(componentLabel).join(", "),
      metadata.arch,
    ].join(" · "),
  };
}
// Mirrors `model::COMPONENTS` public ports: each service is published at port_base + offset.
export const publicServices: Record<
  string,
  { offset: number; path: string; type: string; label?: string }
> = {
  keystone: { offset: 0, path: "/v3", type: "Identity API · v3" },
  glance: { offset: 1, path: "", type: "Image API" },
  placement: { offset: 2, path: "", type: "Placement API" },
  horizon: { offset: 3, path: "/", type: "Dashboard" },
  neutron: { offset: 4, path: "", type: "Network API" },
  nova: { offset: 5, path: "/v2.1", type: "Compute API" },
  heat: { offset: 9, path: "", type: "Orchestration API" },
  "heat-cfn": {
    offset: 10,
    path: "",
    type: "CFN 호환 API",
    label: "Heat CloudFormation",
  },
  barbican: { offset: 11, path: "", type: "Key Manager API" },
  designate: { offset: 12, path: "", type: "DNS API" },
  magnum: { offset: 13, path: "", type: "Container Infra API" },
  skyline: { offset: 14, path: "", type: "Dashboard API" },
  "skyline-console": {
    offset: 15,
    path: "",
    type: "Dashboard",
    label: "Skyline Console",
  },
};
/** The public ports each service publishes, by port id: Heat also serves CloudFormation and
 * Skyline its console, each at its own offset. */
export const componentPorts: Record<string, string[]> = {
  keystone: ["keystone"],
  glance: ["glance"],
  placement: ["placement"],
  horizon: ["horizon"],
  neutron: ["neutron"],
  nova: ["nova"],
  heat: ["heat", "heat-cfn"],
  barbican: ["barbican"],
  designate: ["designate"],
  magnum: ["magnum"],
  skyline: ["skyline", "skyline-console"],
};
/** Offsets of the data stores every stackbox publishes in its block (`model::INFRA_PORTS`). */
export const dataStoreOffsets = [6, 7, 8];
/** The public ports a service list publishes, in offset order, each with its component. */
export function stackboxPublicPorts(components: string[]) {
  return components
    .flatMap((component) =>
      (componentPorts[component] ?? []).map((id) => ({
        component,
        id,
        ...publicServices[id],
      })),
    )
    .sort((a, b) => a.offset - b.offset);
}
/** Stackboxes without a recorded plan run the original Keystone and Glance pair. */
export const stackboxComponents = (stackbox: Stackbox) =>
  stackbox.plan?.components ?? ["keystone", "glance"];
/** Required components, the selection and all their dependencies, in catalog order. */
export function resolveComponents(
  catalog: { id: string; required: boolean; depends_on: string[] }[],
  selected: string[],
): string[] {
  const wanted = new Set<string>();
  const queue = [
    ...catalog.filter((c) => c.required).map((c) => c.id),
    ...selected,
  ];
  while (queue.length) {
    const id = queue.pop()!;
    if (wanted.has(id)) continue;
    wanted.add(id);
    queue.push(...(catalog.find((c) => c.id === id)?.depends_on ?? []));
  }
  return catalog.map((c) => c.id).filter((id) => wanted.has(id));
}
export const remoteAgentNeedsUpdate = (host: RemoteHost) =>
  Boolean(host.agent && !host.agent.artifact?.supports_single_vm);
/** Whether a stackbox can be created on the host now. A remote host also needs an agent
 * from this coordinator's build; the stackbox's job re-inspects it if the last check is old. */
export function hostReady(host: RemoteHost, coordinatorBuildId?: string) {
  if (host.status !== "ready" || host.active_job) return false;
  if (host.reachability?.reachable === false) return false;
  if (host.kind === "local") return true;
  if (host.kind === "agent") return host.connected;
  return (
    host.agent?.artifact?.supports_single_vm === true &&
    !agentBuildMismatch(host.agent?.artifact, coordinatorBuildId)
  );
}
/** Seconds after which the coordinator's last attempt to reach a host is too old to rely on. */
export const REACHABILITY_STALE_SECONDS = 90;
/** What the coordinator's last attempt to reach a host says, for display.
 * `stale` means the check is too old to say anything; it never claims the host is down. */
export type HostLink = "unchecked" | "stale" | "disconnected" | "reachable";
export function hostLink(
  host: RemoteHost,
  nowSeconds = Date.now() / 1000,
): HostLink {
  const reach = host.reachability;
  if (!reach) return "unchecked";
  // A check time the coordinator never wrote reads as the oldest possible.
  if (nowSeconds - (reach.checked_at ?? 0) > REACHABILITY_STALE_SECONDS)
    return "stale";
  return reach.reachable ? "reachable" : "disconnected";
}
/** What still stands between the host and a new stackbox. Preparation installs the runner,
 * the stackbox kernel and the verification VM in one job, so there is one button to press. */
export function hostNotice(host: RemoteHost, coordinatorBuildId?: string) {
  if (host.kind === "local")
    return `Hosts에서 로컬 호스트를 점검하고 환경 준비를 마치세요. msb ${MSB_REQUIRED_VERSION}이 필요합니다.`;
  if (host.kind === "agent")
    return host.connected
      ? `Hosts에서 이 호스트를 점검하고 환경 준비를 마치세요. 호스트에 msb ${MSB_REQUIRED_VERSION}이 필요합니다.`
      : "에이전트가 연결되어 있지 않습니다. 호스트에서 `stackbox agent connect`를 실행하세요.";
  if (agentBuildMismatch(host.agent?.artifact, coordinatorBuildId))
    return "설치된 Stackbox 실행기가 이 coordinator와 다른 소스에서 빌드되었습니다. 환경 준비를 다시 실행하면 이 빌드의 실행기로 바뀝니다. 그래도 실패하면 `task agent-build` 후 Coordinator를 재시작하세요.";
  if (remoteAgentNeedsUpdate(host))
    return "설치된 Stackbox 실행기는 단일 VM 생성을 지원하지 않습니다. Hosts에서 환경 준비를 실행하면 최신 실행기까지 함께 설치합니다. 기존 Stackbox의 조회와 삭제는 계속 사용할 수 있습니다.";
  return "Hosts에서 환경 준비를 실행하면 단일 VM Stackbox 실행기까지 함께 설치됩니다. 준비 상태를 확인하세요.";
}
/** The id of the kernel stackbox installed on a host: its directory under libkrunfw. */
export const customKernelId = (path?: string | null) =>
  path?.split("/").filter(Boolean).slice(-2, -1)[0];
/** One line about the guest kernel a host runs, for places that explain the mtty gate. */
export function hostKernelNote(host: RemoteHost) {
  const inspection = host.inspection;
  if (!inspection) return "호스트를 아직 점검하지 않았습니다.";
  if (inspection.custom_kernel) {
    const id = customKernelId(inspection.custom_kernel);
    return `stackbox 커널${id ? ` ${id}` : ""}로 부팅합니다.`;
  }
  return inspection.custom_kernel_reason
    ? `stock 커널로 부팅합니다: ${inspection.custom_kernel_reason}`
    : "stock 커널로 부팅합니다. stackbox 커널을 설치하지 않았습니다.";
}
export const hostLabel = (host: RemoteHost) =>
  `${host.id} · ${host.kind === "local" ? "로컬" : host.kind === "agent" ? "에이전트" : "원격"}`;
