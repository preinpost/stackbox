import type { Farm, Job, Phase, RemoteHost } from "./api/client";
export const jobKindLabels: Record<Job["kind"], string> = {
  create: "VM 생성",
  deploy: "OpenStack 배포",
  delete: "Farm 삭제",
  snapshot: "스냅샷 얼리기",
  restore: "스냅샷에서 복원",
  settle: "서비스 응답 확인",
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
const osLabels: Record<string, string> = { macos: "macOS", linux: "Linux" };
export const platform = (os: string, arch: string) =>
  `${osLabels[os] ?? os} / ${arch}`;
export const farmMemory = (farm: Farm) =>
  farm.plan?.memory_mib ?? farm.nodes.reduce((sum, n) => sum + n.memory_mib, 0);
/** Recorded roles are preserved for legacy farms; only new all-in-one farms support deployment. */
export const isAllInOneFarm = (farm: Farm) => {
  const roles = farm.plan?.nodes ?? farm.nodes;
  return roles.length === 1 && String(roles[0].role) === "all-in-one";
};
export const farmNodeCount = (farm: Farm) =>
  farm.plan?.nodes.length ?? farm.nodes.length;
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
};
export const componentLabel = (id: string) => componentInfo[id]?.label ?? id;
// Mirrors `model::COMPONENTS` public ports: each service is published at port_base + offset.
export const publicServices: Record<
  string,
  { offset: number; path: string; type: string }
> = {
  keystone: { offset: 0, path: "/v3", type: "Identity API · v3" },
  glance: { offset: 1, path: "", type: "Image API" },
  placement: { offset: 2, path: "", type: "Placement API" },
  horizon: { offset: 3, path: "/", type: "Dashboard" },
  neutron: { offset: 4, path: "", type: "Network API" },
  nova: { offset: 5, path: "/v2.1", type: "Compute API" },
};
/** Farms without a recorded plan run the original Keystone and Glance pair. */
export const farmComponents = (farm: Farm) =>
  farm.plan?.components ?? ["keystone", "glance"];
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
/** Whether a farm can be created on the host now. A remote host also needs an agent
 * from this coordinator's build and an inspection from the last five minutes. */
export function hostReady(host: RemoteHost, coordinatorBuildId?: string) {
  if (host.status !== "ready" || host.active_job) return false;
  if (host.kind === "local") return true;
  if (host.kind === "agent") return host.connected;
  return (
    host.agent?.artifact?.supports_single_vm === true &&
    !agentBuildMismatch(host.agent?.artifact, coordinatorBuildId) &&
    Date.now() / 1000 - (host.inspection?.checked_at ?? 0) <= 300
  );
}
/** What still stands between the host and a new farm. */
export function hostNotice(host: RemoteHost, coordinatorBuildId?: string) {
  if (host.kind === "local")
    return "Hosts에서 로컬 호스트를 점검하고 환경 준비를 마치세요. msb 0.7.8과 golden image(`task docker-build`)가 필요합니다.";
  if (host.kind === "agent")
    return host.connected
      ? "Hosts에서 이 호스트를 점검하고 환경 준비를 마치세요. 호스트에 msb 0.7.8과 golden image(`task docker-build`)가 필요합니다."
      : "에이전트가 연결되어 있지 않습니다. 호스트에서 `farm agent connect`를 실행하세요.";
  if (agentBuildMismatch(host.agent?.artifact, coordinatorBuildId))
    return "설치된 Farm 실행기가 이 coordinator와 다른 소스에서 빌드되었습니다. `task agent-build`로 다시 빌드한 뒤 Hosts에서 실행기를 재설치하세요.";
  if (remoteAgentNeedsUpdate(host))
    return "설치된 Farm 실행기는 단일 VM 생성을 지원하지 않습니다. Hosts에서 최신 Farm 실행기를 설치하세요. 기존 Farm의 조회와 삭제는 계속 사용할 수 있습니다.";
  return "Hosts에서 환경 준비와 단일 VM용 Farm 실행기 설치를 완료하세요. 마지막 SSH 점검이 5분을 지났다면 점검을 갱신하세요.";
}
export const hostLabel = (host: RemoteHost) =>
  `${host.id} · ${host.kind === "local" ? "로컬" : host.kind === "agent" ? "에이전트" : "원격"}`;
