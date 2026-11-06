import { Tabs } from "@base-ui/react/tabs";
import { lazy, Suspense, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useParams } from "@tanstack/react-router";
import {
  ArrowLeft,
  ArrowUpRight,
  Boxes,
  Check,
  CheckCheck,
  Circle,
  CircleAlert,
  Cpu,
  Database,
  Eye,
  EyeOff,
  Globe,
  LoaderCircle,
  RefreshCw,
  KeyRound,
  MessageSquare,
  MonitorDot,
  HardDrive,
  Network,
  Server,
  ShieldCheck,
  Snowflake,
  SquareTerminal,
} from "lucide-react";
import {
  api,
  farmQuery,
  hostsQuery,
  jobsQuery,
  keys,
  type Datastore,
  type Farm,
  type Impair,
  type Job,
} from "../api/client";
import {
  AddComputeDialog,
  CheckServicesButton,
  DeleteFarmDialog,
  DeployFarmDialog,
} from "../components/farm-actions";
import { FreezeFarmDialog } from "../components/snapshot-actions";
import { LogViewer } from "../components/log-viewer";
import {
  Button,
  CopyText,
  EmptyState,
  ErrorNotice,
  Loading,
  StatusBadge,
} from "../components/ui";
import {
  componentLabel,
  dateTime,
  farmComponents,
  farmDeployed,
  farmMemory,
  farmState,
  runningVms,
  isAllInOneFarm,
  isDeployableFarm,
  isMultiNodeFarm,
  memory,
  nodeAddress,
  nodeHostname,
  nodeSuffix,
  pendingCompute,
  phases,
  publicServices,
} from "../lib";
import { shellTargets } from "../components/shell-targets";
import { JobRow } from "./jobs";
import { StepProgress } from "../components/job-steps";

// xterm is large; load it only when a shell is opened.
const FarmTerminal = lazy(() =>
  import("../components/farm-terminal").then((m) => ({
    default: m.FarmTerminal,
  })),
);

export function FarmDetailRoute() {
  const { farmId } = useParams({ from: "/farms/$farmId" });
  return <FarmDetailPage key={farmId} id={farmId} />;
}
export function FarmDetailPage({ id }: { id: string }) {
  const query = useQuery(farmQuery(id));
  const jobs = useQuery(jobsQuery());
  const hosts = useQuery(hostsQuery());
  const [tab, setTab] = useState("overview");
  // The shell connects when its tab is first opened and stays up across tab changes.
  const [shellOpened, setShellOpened] = useState(false);
  if (query.isPending) return <Loading label="Farm 정보를 불러오는 중" />;
  if (query.error)
    return (
      <>
        <Link to="/" className="back-link">
          <ArrowLeft size={15} />
          Farms
        </Link>
        <ErrorNotice
          error={query.error}
          retry={() => {
            void query.refetch();
          }}
        />
      </>
    );
  const farm = query.data;
  const history = [...(jobs.data ?? [])]
    .filter((j) => j.farm_id === id)
    .sort((a, b) => b.created_at - a.created_at);
  const latestDeployment = history.find((j) => j.kind === "deploy");
  // A compute addition that stopped midway: its VM may run while the plan never kept it.
  const failedExpansion = history.find(
    (j) =>
      j.kind === "expand" &&
      ["failed", "interrupted"].includes(j.status) &&
      Boolean(j.target_node),
  );
  const resumableExpansion =
    Boolean(failedExpansion) &&
    ["failed", "broken"].includes(farm.status) &&
    !farm.active_job;
  const activeJob = history.find((j) => j.id === farm.active_job);
  // A remote farm keeps the agent digest it was created with; updates need a new farm.
  const host = hosts.data?.find((h) => h.id === farm.host_id);
  const staleAgent = Boolean(
    farm.agent_sha256 &&
    host?.agent?.artifact?.sha256 &&
    farm.agent_sha256 !== host.agent.artifact.sha256,
  );
  // A restored farm was deployed before freezing; its VM starts from disk without a deploy job.
  const deployed =
    history.some((j) => j.kind === "deploy" && j.status === "succeeded") ||
    Boolean(farm.snapshot_id);
  return (
    <>
      <Link to="/" className="back-link">
        <ArrowLeft size={15} />
        모든 farm
      </Link>
      <div className="detail-heading">
        <div className="detail-title">
          <span className="detail-avatar">
            <Boxes size={27} strokeWidth={1.6} />
          </span>
          <div>
            <div className="flex flex-wrap items-center gap-3">
              <h1>{farm.id}</h1>
              <StatusBadge status={farmState(farm, activeJob)} />
            </div>
            <p>
              {farm.owner || "담당자 미지정"}
              <span>·</span>
              {!farm.host_id
                ? "호스트 없음"
                : host?.kind === "local"
                  ? `로컬 · ${farm.host_id}`
                  : `원격 · ${farm.host_id}`}
              <span>·</span>
              {dateTime(farm.created_at)} 생성
              {farm.snapshot_id && (
                <>
                  <span>·</span>
                  <Link to="/snapshots" className="hover:underline">
                    스냅샷 {farm.snapshot_id}에서 복원
                  </Link>
                </>
              )}
            </p>
          </div>
        </div>
        <div className="flex gap-2">
          {farm.status !== "deleted" && (
            <>
              <DeployFarmDialog farm={farm} lastJob={latestDeployment} />
              <AddComputeDialog
                farm={farm}
                lastJob={history.find((j) => j.kind === "expand")}
              />
              <CheckServicesButton farm={farm} deployed={deployed} />
              <FreezeFarmDialog farm={farm} />
              <DeleteFarmDialog farm={farm} />
            </>
          )}
        </div>
      </div>
      {!isDeployableFarm(farm) && farm.status !== "deleted" && (
        <div className="warning-notice mb-5">
          기존 다중 VM 구성입니다. 이 Farm의 상태 조회와 삭제는 계속 사용할 수
          있습니다. 배포하려면 Farm을 새로 만드세요.
        </div>
      )}
      {staleAgent && (
        <div className="info-notice">
          <RefreshCw size={18} />
          <div>
            <strong>이 farm은 예전 Farm 실행기에 고정되어 있습니다.</strong>{" "}
            배포 진행 상황은 그 실행기가 보내는 단계까지만 표시됩니다. 새
            실행기를 쓰려면 같은 구성으로 farm을 새로 만드세요.
          </div>
        </div>
      )}
      {farm.observation_error && (
        <div role="alert" className="warning-notice mb-5">
          {farm.observation_error}
          <p className="mt-1 text-xs">
            마지막으로 확인한 VM 상태를 표시합니다. 메모리와 포트 예약은
            유지됩니다.
            {farm.last_seen_at &&
              ` · 마지막 확인 ${dateTime(farm.last_seen_at)}`}
          </p>
        </div>
      )}
      {farm.error && (
        <div className="error-notice">
          <CircleAlert size={18} className="shrink-0" />
          <div>
            <strong>
              {farm.status === "broken"
                ? "VM 상태를 확인하세요"
                : "마지막 작업을 완료하지 못했습니다"}
            </strong>
            <p>{farm.error}</p>
            {resumableExpansion && failedExpansion && (
              <p>
                실패한 compute 추가:{" "}
                <span className="font-mono">{failedExpansion.target_node}</span>
                . Compute 추가에서 같은 VM·같은 방식으로 이어서 재시도할 수
                있습니다.
              </p>
            )}
            {!resumableExpansion && latestDeployment?.phase && (
              <p>
                마지막 배포 단계:{" "}
                <span className="font-mono">{latestDeployment.phase}</span>.
                배포 재개에서 시작 단계를 선택할 수 있습니다.
              </p>
            )}
          </div>
        </div>
      )}
      {activeJob?.kind === "snapshot" && (
        <div className="info-notice">
          <Snowflake size={18} />
          <div>
            <strong>스냅샷 {activeJob.snapshot_id}을 만드는 중입니다.</strong>{" "}
            VM을 잠시 정지했다가 다시 시작합니다. 그동안 서비스가 응답하지
            않습니다.
            <div className="mt-2">
              <StepProgress job={activeJob} />
            </div>
          </div>
        </div>
      )}
      {activeJob?.kind === "restore" && (
        <div className="info-notice">
          <Snowflake size={18} />
          <div>
            <strong>
              스냅샷 {activeJob.snapshot_id}에서 복원하는 중입니다.
            </strong>{" "}
            VM이 부팅되면 public endpoint를 이 farm의 포트로 옮기고 서비스가
            응답할 때까지 기다립니다.
          </div>
        </div>
      )}
      {activeJob?.kind === "expand" && (
        <div className="info-notice">
          <Cpu size={18} />
          <div>
            <strong>{activeJob.target_node ?? "compute"} 추가 중입니다.</strong>{" "}
            호스트에 VM을 마련하고 control VM의 Kolla inventory에 등록한 뒤, 그
            VM에만 배포합니다. 기존 서비스는 그대로 동작합니다.
          </div>
        </div>
      )}
      {farm.status === "created" &&
        !farm.active_job &&
        isDeployableFarm(farm) && (
          <div className="info-notice">
            <ShieldCheck size={18} />
            <div>
              <strong>VM이 준비되었습니다.</strong> OpenStack 배포를 시작하면
              API 서비스를 사용할 수 있습니다.
            </div>
          </div>
        )}
      {farm.status === "deleted" && (
        <div className="info-notice">
          <CheckCheck size={18} />
          <span>
            모든 VM이 삭제되었습니다. 작업 내역과 로그는 계속 확인할 수
            있습니다.
          </span>
        </div>
      )}
      <div className="detail-facts">
        <div>
          <Cpu size={17} />
          <span>
            메모리<strong>{memory(farmMemory(farm))}</strong>
          </span>
        </div>
        <div>
          <Server size={17} />
          <span>
            가상 머신
            <strong>{runningVms(farm, activeJob)}</strong>
          </span>
        </div>
        <div>
          <HardDrive size={17} />
          <span>
            할당 디스크
            <strong>
              {farm.plan ? memory(farm.plan.disk_mib) : "기존 VM 구성"}
            </strong>
          </span>
        </div>
        <div>
          <Globe size={17} />
          <span>
            Public host<strong>{farm.exposure?.host ?? "내부 접속"}</strong>
          </span>
        </div>
      </div>
      <Tabs.Root
        value={tab}
        onValueChange={(value: string) => {
          setTab(value);
          if (value === "shell") setShellOpened(true);
        }}
        className="detail-tabs"
      >
        <Tabs.List className="tabs-list" aria-label="Farm 상세 메뉴">
          <Tabs.Tab value="overview" className="tab">
            Overview
          </Tabs.Tab>
          <Tabs.Tab value="logs" className="tab">
            Logs{farm.active_job && <span className="online-dot" />}
          </Tabs.Tab>
          <Tabs.Tab value="jobs" className="tab">
            Jobs<span className="count-pill">{history.length}</span>
          </Tabs.Tab>
          <Tabs.Tab value="shell" className="tab">
            Shell
          </Tabs.Tab>
        </Tabs.List>
        <Tabs.Panel value="overview" className="tab-panel">
          <NodeTopology farm={farm} job={activeJob} />
          <div className="detail-bottom-grid">
            <DeploymentPhases farm={farm} />
            <Endpoints farm={farm} />
          </div>
          <Datastores farm={farm} />
        </Tabs.Panel>
        <Tabs.Panel value="logs" className="tab-panel">
          <LogViewer id={id} />
        </Tabs.Panel>
        <Tabs.Panel value="jobs" className="tab-panel">
          <section className="panel">
            {jobs.error ? (
              <ErrorNotice
                error={jobs.error}
                retry={() => {
                  void jobs.refetch();
                }}
              />
            ) : jobs.isPending ? (
              <Loading />
            ) : history.length ? (
              history.map((job) => <JobRow job={job} key={job.id} />)
            ) : (
              <EmptyState
                icon={<Circle size={24} />}
                title="기록된 작업이 없습니다"
                description="이 farm에서 시작한 작업이 여기에 표시됩니다."
              />
            )}
          </section>
        </Tabs.Panel>
        <Tabs.Panel value="shell" className="tab-panel" keepMounted>
          {!shellTargets(farm.nodes).some(
            ({ node }) => node.status === "running",
          ) ? (
            <section className="panel">
              <EmptyState
                icon={<SquareTerminal size={24} />}
                title="VM이 실행 중이 아닙니다"
                description={
                  isAllInOneFarm(farm)
                    ? "VM이 실행 중일 때 셸을 열 수 있습니다."
                    : isMultiNodeFarm(farm)
                      ? "VM이 실행 중일 때 셸을 열 수 있습니다."
                      : "셸은 기존 다중 VM farm에서 지원하지 않습니다."
                }
              />
            </section>
          ) : (
            shellOpened && (
              <Suspense fallback={<Loading label="터미널을 불러오는 중" />}>
                <FarmTerminal farm={farm} active={tab === "shell"} />
              </Suspense>
            )
          )}
        </Tabs.Panel>
      </Tabs.Root>
    </>
  );
}
const roleInfo: Record<string, { label: string; service: string }> = {
  "all-in-one": {
    label: "All-in-one",
    service: "Kolla Ansible · DB · MQ · Cache",
  },
  deployer: { label: "Deployer", service: "Kolla Ansible" },
  lb: { label: "Load balancer", service: "HAProxy" },
  infra: { label: "Infrastructure", service: "DB · MQ · Cache" },
  keystone: { label: "Keystone", service: "Identity API" },
  glance: { label: "Glance", service: "Image API" },
  control: { label: "Control", service: "" },
  compute: { label: "Compute", service: "" },
};
const controlServices = ["placement", "horizon", "neutron", "nova"];
/** Faults the topology offers for a multi-node VM; the last entry heals it. */
const faultPresets: { label: string; impair: Impair }[] = [
  { label: "고립", impair: { loss: 1 } },
  { label: "손실 5%", impair: { loss: 0.05 } },
  { label: "지연 50 ms", impair: { delay_ms: 50 } },
  { label: "복구", impair: {} },
];
function faultLabel(impair?: Impair) {
  if (!impair) return null;
  if ((impair.loss ?? 0) >= 1) return "고립됨";
  return [
    impair.delay_ms ? `지연 ${impair.delay_ms} ms` : null,
    impair.loss ? `손실 ${Math.round(impair.loss * 100)}%` : null,
  ]
    .filter(Boolean)
    .join(" · ");
}
function NodeFaults({ farm, node }: { farm: Farm; node: string }) {
  const client = useQueryClient();
  const mutation = useMutation({
    mutationFn: (impair: Impair) => api.setFaults(farm.id, node, impair),
    onSuccess: (updated) => {
      client.setQueryData(keys.farm(farm.id), updated);
      void client.invalidateQueries({ queryKey: keys.farms });
    },
  });
  const current = faultLabel(farm.faults?.[node]);
  const usable = !["creating", "deleting", "deleted"].includes(farm.status);
  return (
    <div className="node-faults">
      <span className={`fault-chip ${current ? "faulted" : ""}`}>
        {current ?? "링크 정상"}
      </span>
      <div className="node-fault-actions">
        {faultPresets.map(({ label, impair }) => (
          <Button
            key={label}
            variant={impair.loss === 1 ? "danger" : "ghost"}
            disabled={
              !usable ||
              mutation.isPending ||
              (!current && !impair.loss && !impair.delay_ms)
            }
            onClick={() => mutation.mutate(impair)}
          >
            {label}
          </Button>
        ))}
      </div>
      {mutation.error && <ErrorNotice error={mutation.error} />}
    </div>
  );
}
/** A control VM and its computes on the farm's management and tenant links. */
function MultiNodeTopology({ farm, job }: { farm: Farm; job?: Job }) {
  const components = farmComponents(farm);
  const specs = farm.plan?.nodes ?? [];
  // A compute being added joins the drawing before Kolla lets the farm keep it.
  const pending = pendingCompute(farm, job);
  const exposure = farm.status !== "deleted" ? farm.exposure : null;
  return (
    <section className="panel topology">
      <div className="panel-toolbar">
        <h2 className="flex items-center gap-2">
          <Network size={17} />
          VM topology
        </h2>
        <span className="topology-network">
          {`mgmt 10.254.0.0/24 · tenant VLAN · VM ${specs.length + (pending ? 1 : 0)}대`}
          {pending && " · 1대 생성 중"}
          <span className="status-dot" />
        </span>
      </div>
      <div className="topology-graph">
        <div className="topology-box topology-host">
          <Globe size={16} strokeWidth={1.6} />
          <div>
            <small>Host · link switch</small>
            <code>{exposure?.host ?? "public 포트 없음"}</code>
          </div>
        </div>
        <div className="topology-edge edge-active">
          <code>farm0 · farm1</code>
          <span className="edge-line" aria-hidden />
          <small>WebSocket 링크</small>
        </div>
        <div className="topology-tree">
          {specs.map((spec) => {
            const node = farm.nodes.find(
              (n) =>
                n.role === spec.role &&
                (n.index ?? null) === (spec.index ?? null),
            );
            const suffix = nodeSuffix(spec);
            const control = spec.role === "control";
            return (
              <div className="topology-leaf" key={suffix}>
                <div
                  className={`topology-node ${node?.status === "running" ? "node-running" : ""}`}
                >
                  <div className="node-top">
                    <Server size={15} strokeWidth={1.6} />
                    <strong className="font-mono">{nodeHostname(spec)}</strong>
                    <span className="node-memory">
                      {memory(spec.memory_mib)}
                    </span>
                    <span
                      className={`node-status ${node?.status === "running" ? "running" : ""}`}
                      title={node?.status ?? "준비되지 않음"}
                    />
                  </div>
                  <p>
                    {control
                      ? components
                          .filter((c) => c !== "nova")
                          .map(componentLabel)
                          .concat("Nova API", "L3·DHCP", "Kolla")
                          .join(" · ")
                      : "nova-compute · libvirt · OVS agent"}
                  </p>
                  <div className="node-meta">
                    <code>{nodeAddress(spec)}</code>
                    <span className="font-mono">
                      {node?.name ?? `${farm.id}-${suffix}`}
                    </span>
                  </div>
                  <NodeFaults farm={farm} node={suffix} />
                </div>
              </div>
            );
          })}
          {pending && (
            <div className="topology-leaf" key={nodeSuffix(pending)}>
              <div className="topology-node pending">
                <div className="node-top">
                  <Server size={15} strokeWidth={1.6} />
                  <strong className="font-mono">{nodeHostname(pending)}</strong>
                  <span className="node-memory">
                    {memory(pending.memory_mib)}
                  </span>
                  <span
                    className="node-status running"
                    title="생성 중"
                    aria-hidden
                  />
                </div>
                <p>nova-compute · libvirt · OVS agent</p>
                <div className="node-meta">
                  <code>{nodeAddress(pending)}</code>
                  <span className="font-mono">
                    {farm.id}-{nodeSuffix(pending)}
                  </span>
                </div>
                <div className="node-pending">
                  <LoaderCircle size={13} className="animate-spin" />
                  생성 중{pending.host && <span>· {pending.host}</span>}
                </div>
              </div>
            </div>
          )}
        </div>
      </div>
      <div className="topology-footer">
        <ShieldCheck size={13} />
        고립은 VM의 두 링크에서 모든 프레임을 버립니다. compute VM을 고립시키면
        Nova가 약 1분 뒤 down으로 표시하고, 그 뒤 evacuate를 시험할 수 있습니다.
      </div>
    </section>
  );
}
export function NodeTopology({ farm, job }: { farm: Farm; job?: Job }) {
  if (isMultiNodeFarm(farm)) return <MultiNodeTopology farm={farm} job={job} />;
  const single = isAllInOneFarm(farm);
  const components = farmComponents(farm);
  const sources = farm.plan
    ? farm.plan.nodes.map((spec) => ({
        role: spec.role,
        node: farm.nodes.find((node) => node.role === spec.role),
      }))
    : farm.nodes.map((node) => ({ role: node.role ?? "unknown", node }));
  const roles = sources.map(({ role, node }) => ({
    role,
    node,
    label: roleInfo[role]?.label ?? role,
    service:
      role === "control"
        ? components
            .filter((c) => controlServices.includes(c))
            .map(componentLabel)
            .join(" · ")
        : role === "compute"
          ? [
              components.includes("nova") && "Hypervisor",
              components.includes("neutron") && "OVS · Neutron agents",
            ]
              .filter(Boolean)
              .join(" · ")
          : (roleInfo[role]?.service ?? ""),
  }));
  const exposure = farm.status !== "deleted" ? farm.exposure : null;
  const publicPort = (id: string) =>
    exposure && publicServices[id]
      ? exposure.port_base + publicServices[id].offset
      : null;
  const ports = components
    .map(publicPort)
    .filter((port): port is number => port !== null);
  const portRange = !ports.length
    ? "—"
    : ports.length === 1
      ? `:${ports[0]}`
      : `:${Math.min(...ports)}–${Math.max(...ports)}`;
  const running = farm.nodes.some((node) => node.status === "running");
  const nodeMemory = (node: Farm["nodes"][number] | undefined) =>
    node
      ? memory(node.memory_mib)
      : farm.status === "deleted"
        ? "삭제됨"
        : "생성 대기";
  return (
    <section className="panel topology">
      <div className="panel-toolbar">
        <h2 className="flex items-center gap-2">
          <Network size={17} />
          VM topology
        </h2>
        <span className="topology-network">
          {single ? "farm0 10.254.0.2 · 단일 VM" : "기존 다중 VM 구성"}
          <span className="status-dot" />
        </span>
      </div>
      <div className="topology-graph">
        <div className="topology-box topology-host">
          <Globe size={16} strokeWidth={1.6} />
          <div>
            <small>Host</small>
            <code>{exposure?.host ?? "public 포트 없음"}</code>
          </div>
        </div>
        <div
          className={`topology-edge ${running && exposure ? "edge-active" : ""}`}
        >
          <code>{portRange}</code>
          <span className="edge-line" aria-hidden />
          <small>{exposure ? "port bind · DNAT" : "노출 안 함"}</small>
        </div>
        {single ? (
          roles.map(({ role, node, service }) => (
            <div
              key={node?.name ?? role}
              className={`topology-node topology-vm ${node?.status === "running" ? "node-running" : ""}`}
            >
              <div className="topology-vm-header">
                <Server size={16} strokeWidth={1.6} />
                <strong className="font-mono">
                  {node?.name ?? `${farm.id}-${role}`}
                </strong>
                <span className="node-memory">{nodeMemory(node)}</span>
                <span
                  className={`node-status ${node?.status === "running" ? "running" : ""}`}
                  title={node?.status ?? "준비되지 않음"}
                />
              </div>
              <div className="topology-vm-body">
                <div
                  className="topology-box topology-iface"
                  title="고정 서비스 주소"
                >
                  <Network size={15} strokeWidth={1.6} />
                  <div>
                    <small>farm0</small>
                    <code>10.254.0.2</code>
                  </div>
                </div>
                <div
                  className="topology-tree"
                  aria-label="같은 VM에서 실행할 서비스"
                >
                  {components.map((id) => (
                    <div className="topology-leaf" key={id}>
                      <span className="topology-box topology-service">
                        <span>{componentLabel(id)}</span>
                        {publicPort(id) !== null && (
                          <code>:{publicPort(id)}</code>
                        )}
                      </span>
                    </div>
                  ))}
                </div>
              </div>
              <div className="topology-vm-footer">
                <span>기반 서비스</span>
                {service}
              </div>
            </div>
          ))
        ) : (
          <div className="topology-tree">
            {roles.map(({ role, node, label, service }, index) => (
              <div
                className="topology-leaf"
                key={node?.name ?? `${role}-${index}`}
              >
                <div
                  className={`topology-node ${node?.status === "running" ? "node-running" : ""}`}
                >
                  <div className="node-top">
                    <Server size={15} strokeWidth={1.6} />
                    <strong>{label}</strong>
                    <span
                      className={`node-status ${node?.status === "running" ? "running" : ""}`}
                      title={node?.status ?? "준비되지 않음"}
                    />
                  </div>
                  {service && <p>{service}</p>}
                  <div className="node-name">
                    {node?.name ?? `${farm.id}-${role}`}
                  </div>
                  <div className="node-meta">
                    <code>{node?.ip ?? "—"}</code>
                    <span>{nodeMemory(node)}</span>
                  </div>
                </div>
              </div>
            ))}
          </div>
        )}
      </div>
      <div className="topology-footer">
        <ShieldCheck size={13} />
        {single
          ? "선택한 OpenStack 서비스와 기반 서비스를 이 VM 안에 설치합니다."
          : "기존 VM 구성을 표시합니다. 단일 VM 구성으로 자동 변환하지 않습니다."}
      </div>
    </section>
  );
}
export function DeploymentPhases({ farm }: { farm: Farm }) {
  return (
    <section className="panel phase-panel">
      <div className="section-label">
        <span className="small-icon">
          <CheckCheck size={17} />
        </span>
        <h2>배포 단계</h2>
        {farm.phase && <span className="count-pill ml-auto">{farm.phase}</span>}
      </div>
      <ol className="phase-list">
        {phases.map((phase) => {
          // A snapshot stops the VM without undoing the deployment it finished.
          const ready = farmDeployed(farm);
          const current = farm.phase === phase.id;
          const running = current && Boolean(farm.active_job) && !ready;
          return (
            <li
              key={phase.id}
              className={ready ? "phase-done" : current ? "phase-current" : ""}
            >
              <span className="phase-marker">
                {ready ? <Check size={12} /> : current ? <span /> : null}
              </span>
              <div className="phase-text">
                <div className="phase-heading">
                  <strong>{phase.label}</strong>
                  <span>{phase.description}</span>
                </div>
                {running && farm.step && (
                  <span
                    className="phase-step phase-step-live"
                    title={farm.step}
                  >
                    <LoaderCircle size={11} className="animate-spin" />
                    <span className="phase-step-text">{farm.step}</span>
                  </span>
                )}
              </div>
            </li>
          );
        })}
      </ol>
    </section>
  );
}
function Endpoints({ farm }: { farm: Farm }) {
  const exposure = farm.status !== "deleted" ? farm.exposure : null;
  const deployed = farmDeployed(farm);
  // Credentials come from the VM, which a snapshot has stopped.
  const showLogin =
    farm.status === "ready" && farmComponents(farm).includes("horizon");
  const login = useQuery({
    queryKey: keys.endpoints(farm.id),
    queryFn: () => api.endpoints(farm.id),
    enabled: showLogin,
    staleTime: 60_000,
  });
  const services = exposure
    ? farmComponents(farm)
        .filter((id) => publicServices[id])
        .map((id) => ({
          name: componentLabel(id),
          type: publicServices[id].type,
          url: `http://${exposure.host}:${exposure.port_base + publicServices[id].offset}${publicServices[id].path}`,
        }))
    : [];
  return (
    <section className="panel endpoints-panel">
      <div className="section-label">
        <span className="small-icon">
          <Globe size={17} />
        </span>
        <h2>Service endpoints</h2>
      </div>
      {!services.length ? (
        <div className="endpoint-note">
          {farm.status === "deleted"
            ? "환경이 삭제되어 서비스 주소를 사용할 수 없습니다."
            : "이 환경에는 public 포트가 없습니다. 내부 네트워크 주소로 접속하세요."}
        </div>
      ) : (
        <>
          <p className="field-hint mb-4">
            {deployed
              ? "마지막 배포가 완료된 서비스 주소입니다."
              : "배포 완료 후 사용할 public 서비스 주소입니다."}
          </p>
          <div className="endpoint-list">
            {services.map((s) => (
              <div className="endpoint" key={s.name}>
                <div>
                  <span className="endpoint-icon">
                    {s.name === "Keystone" ? (
                      <ShieldCheck size={17} />
                    ) : (
                      <LayersIcon />
                    )}
                  </span>
                  <div>
                    <strong>{s.name}</strong>
                    <small>{s.type}</small>
                  </div>
                  {deployed ? (
                    <a
                      href={s.url}
                      target="_blank"
                      rel="noreferrer"
                      className="icon-button ml-auto"
                      aria-label={`${s.name} API 열기`}
                    >
                      <ArrowUpRight size={16} />
                    </a>
                  ) : (
                    <span className="endpoint-pending">배포 대기</span>
                  )}
                </div>
                <div className="endpoint-url">
                  <code>{s.url}</code>
                  <CopyText value={s.url} hideValue label={`${s.name} URL`} />
                </div>
                {s.name === "Horizon" && showLogin && (
                  <HorizonLogin
                    credentials={login.data?.credentials}
                    authUrl={services.find((x) => x.name === "Keystone")?.url}
                    pending={login.isPending}
                    error={login.error}
                  />
                )}
              </div>
            ))}
          </div>
        </>
      )}
      {showLogin && (
        <div className="credential-note">
          <ShieldCheck size={16} />
          <p>Horizon 로그인에 위 admin 계정을 사용합니다.</p>
        </div>
      )}
      {farm.expires_at && (
        <div className="expiry-note">
          만료 시각: {dateTime(farm.expires_at)} · 자동 삭제 없음
        </div>
      )}
    </section>
  );
}
/** One muted line per store, pinned to the card's bottom so the cards read alike. */
const storeNotes: Record<string, string> = {
  mariadb: "root 계정 · 배포한 서비스의 스키마가 함께 생성됩니다",
  rabbitmq: "AMQP 0-9-1 · 아래 관리 UI에서도 같은 계정을 씁니다",
  "rabbitmq-management": "웹 콘솔 · openstack 계정으로 로그인합니다",
};
const storeIcons: Record<string, React.ReactNode> = {
  mariadb: <Database size={16} />,
  rabbitmq: <MessageSquare size={16} />,
  "rabbitmq-management": <MonitorDot size={16} />,
};
/**
 * MariaDB and RabbitMQ ship with every farm; integrations connect through their published ports.
 * Shares the endpoints query cache with the service panel above.
 */
function Datastores({ farm }: { farm: Farm }) {
  const deployed = farmDeployed(farm);
  const query = useQuery({
    queryKey: keys.endpoints(farm.id),
    queryFn: () => api.endpoints(farm.id),
    enabled: farm.status === "ready",
    staleTime: 60_000,
  });
  const stores = query.data?.datastores ?? [];
  return (
    <section className="panel datastore-panel">
      <div className="section-label">
        <span className="small-icon">
          <Database size={17} />
        </span>
        <h2>Data stores</h2>
        {deployed && stores.length > 0 && (
          <span className="count-pill ml-auto">{stores.length}</span>
        )}
      </div>
      {!deployed ? (
        <div className="endpoint-note">
          배포가 끝나면 MariaDB와 RabbitMQ 접속 정보가 여기에 표시됩니다.
        </div>
      ) : query.isPending ? (
        <Loading label="데이터 스토어 정보를 불러오는 중" />
      ) : query.error ? (
        <ErrorNotice error={query.error} />
      ) : !stores.length ? (
        <div className="endpoint-note">
          이 farm은 데이터 스토어 포트를 공개하지 않았습니다. 새로 만든 farm부터
          MariaDB·RabbitMQ 포트가 함께 열립니다.
        </div>
      ) : (
        <>
          <p className="field-hint">
            farm마다 같은 VM 안에서 실행되는 데이터베이스와 메시지 큐입니다.
            포트는 {farm.exposure?.port_base ?? "—"}번대 블록의 해당 offset을
            씁니다.
          </p>
          <div className="datastore-list">
            {stores.map((store) => (
              <DatastoreCard store={store} key={store.id} />
            ))}
          </div>
          <div className="credential-note">
            <ShieldCheck size={16} />
            <p>
              이 포트는 farm VM의 고정 주소로 DNAT됩니다. 외부에 공개한
              포털이라면 인증정보 표시를 끄고 방화벽으로 3306·5672·15672를
              제한하세요.
            </p>
          </div>
        </>
      )}
    </section>
  );
}

/** One data store: connection, account, and the schemas a client may open. */
export function DatastoreCard({ store }: { store: Datastore }) {
  const [revealed, setRevealed] = useState(false);
  const console_ = store.protocol === "http";
  return (
    <article className="datastore">
      <header className="datastore-head">
        <span className="datastore-icon">
          {storeIcons[store.id] ?? <Database size={16} />}
        </span>
        <div className="datastore-title">
          <strong>{store.label}</strong>
          <span className="datastore-proto">{store.protocol}</span>
        </div>
        {console_ && (
          <a
            href={store.url}
            target="_blank"
            rel="noreferrer"
            className="icon-button datastore-open"
            aria-label={`${store.label} 열기`}
          >
            <ArrowUpRight size={16} />
          </a>
        )}
      </header>
      <div className="datastore-body">
        <div className="datastore-conn">
          <KeyRound size={14} />
          <code>
            {store.host}:{store.port}
          </code>
          <CopyText value={store.url} hideValue label="URL" />
        </div>
        <dl className="datastore-fields">
          {store.username && (
            <div>
              <dt>사용자</dt>
              <dd>
                <code>{store.username}</code>
              </dd>
            </div>
          )}
          {store.vhost && (
            <div>
              <dt>vhost</dt>
              <dd>
                <code>{store.vhost}</code>
              </dd>
            </div>
          )}
          {store.password && (
            <div className="datastore-wide">
              <dt>
                비밀번호
                <button
                  type="button"
                  className="secret-toggle"
                  aria-label={revealed ? "비밀번호 가리기" : "비밀번호 표시"}
                  aria-pressed={revealed}
                  onClick={() => setRevealed((open) => !open)}
                >
                  {revealed ? "가리기" : "표시"}
                </button>
              </dt>
              <dd className="datastore-secret">
                <code className={revealed ? "" : "masked"}>
                  {revealed ? store.password : "••••••••••••"}
                </code>
                <CopyText value={store.password} hideValue label="비밀번호" />
              </dd>
            </div>
          )}
          {(store.databases?.length ?? 0) > 0 && (
            <div className="datastore-wide">
              <dt>데이터베이스 {store.databases!.length}</dt>
              <dd className="datastore-dbs">
                {store.databases!.map((name) => (
                  <span key={name}>{name}</span>
                ))}
              </dd>
            </div>
          )}
        </dl>
        <p className="datastore-note">{storeNotes[store.id]}</p>
      </div>
    </article>
  );
}

/** Horizon sign-in details, each row copyable, plus an `OS_*` block for the CLI. */
export function HorizonLogin({
  credentials,
  authUrl,
  pending,
  error,
}: {
  credentials?: Record<string, string>;
  authUrl?: string;
  pending: boolean;
  error: unknown;
}) {
  const [revealed, setRevealed] = useState(false);
  if (pending)
    return (
      <p className="horizon-login-note">로그인 정보를 불러오는 중입니다.</p>
    );
  if (error || !credentials?.OS_PASSWORD)
    return (
      <p className="horizon-login-note">
        로그인 정보를 가져오지 못했습니다. 배포가 끝난 뒤 다시 열어 보세요.
      </p>
    );
  const username = credentials.OS_USERNAME || "admin";
  const domain = credentials.OS_USER_DOMAIN_NAME || "Default";
  const project = credentials.OS_PROJECT_NAME || "admin";
  const projectDomain = credentials.OS_PROJECT_DOMAIN_NAME || domain;
  const envBlock = [
    authUrl && `export OS_AUTH_URL=${authUrl}`,
    `export OS_USERNAME=${username}`,
    `export OS_PASSWORD=${credentials.OS_PASSWORD}`,
    `export OS_PROJECT_NAME=${project}`,
    `export OS_USER_DOMAIN_NAME=${domain}`,
    `export OS_PROJECT_DOMAIN_NAME=${projectDomain}`,
    "export OS_IDENTITY_API_VERSION=3",
  ]
    .filter(Boolean)
    .join("\n");
  return (
    <>
      <dl className="horizon-login">
        <LoginRow label="사용자" value={username} />
        <LoginRow
          label="비밀번호"
          value={credentials.OS_PASSWORD}
          secret
          revealed={revealed}
          onToggle={() => setRevealed((open) => !open)}
        />
        <LoginRow label="도메인" value={domain} />
        <LoginRow label="프로젝트" value={project} />
      </dl>
      <div className="horizon-login-actions">
        <CopyText
          value={envBlock}
          label="OS_ 환경변수 복사"
          name="OS_ 환경변수"
        />
      </div>
    </>
  );
}

function LoginRow({
  label,
  value,
  secret = false,
  revealed = false,
  onToggle,
}: {
  label: string;
  value: string;
  secret?: boolean;
  revealed?: boolean;
  onToggle?: () => void;
}) {
  const shown = !secret || revealed;
  return (
    <div>
      <dt>{label}</dt>
      <dd>
        <code className={shown ? "" : "masked"}>
          {shown ? value : "••••••••••••"}
        </code>
        {secret && (
          <button
            type="button"
            className="secret-toggle"
            title={shown ? `${label} 가리기` : `${label} 표시`}
            aria-label={shown ? `${label} 가리기` : `${label} 표시`}
            aria-pressed={shown}
            onClick={onToggle}
          >
            {shown ? <EyeOff size={14} /> : <Eye size={14} />}
          </button>
        )}
        <CopyText value={value} hideValue label={label} />
      </dd>
    </div>
  );
}

function LayersIcon() {
  return <Boxes size={17} />;
}
