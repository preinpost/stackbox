import { Tabs } from "@base-ui/react/tabs";
import { lazy, Suspense, useEffect, useRef, useState } from "react";
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
  CircuitBoard,
  Cpu,
  Database,
  Eye,
  EyeOff,
  Globe,
  LoaderCircle,
  MoreHorizontal,
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
  Terminal,
} from "lucide-react";
import {
  api,
  stackboxQuery,
  hostsQuery,
  jobsQuery,
  keys,
  logsQuery,
  useDisableRelayExposure,
  useEnableRelayExposure,
  type Datastore,
  type Stackbox,
  type Impair,
  type Job,
  type RelayExposure,
  type RemoteHost,
} from "../api/client";
import {
  AddComputeDialog,
  CheckServicesButton,
  DeleteStackboxDialog,
  DeployStackboxDialog,
} from "../components/stackbox-actions";
import { FreezeStackboxDialog } from "../components/snapshot-actions";
import { GpuLoadPanel } from "../components/gpu-load";
import { StackboxRelease, stackboxReleaseLabel } from "../components/stackbox-release";
import { LogLines, LogViewer, useLogStream } from "../components/log-viewer";
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
  stackboxComponents,
  stackboxDeployed,
  stackboxMemory,
  stackboxState,
  runningVms,
  isAllInOneStackbox,
  isDeployableStackbox,
  isMultiNodeStackbox,
  memory,
  nodeAddress,
  nodeHostname,
  nodeSuffix,
  pendingCompute,
  phases,
  dataStoreOffsets,
  stackboxPublicPorts,
} from "../lib";
import { shellTargets } from "../components/shell-targets";
import { JobRow } from "./jobs";
import { StepProgress } from "../components/job-steps";

// xterm is large; load it only when a shell is opened.
const StackboxTerminal = lazy(() =>
  import("../components/stackbox-terminal").then((m) => ({
    default: m.StackboxTerminal,
  })),
);

export function StackboxDetailRoute() {
  const { stackboxId } = useParams({ from: "/stackboxes/$stackboxId" });
  return <StackboxDetailPage key={stackboxId} id={stackboxId} />;
}
export function StackboxDetailPage({ id }: { id: string }) {
  const query = useQuery(stackboxQuery(id));
  const jobs = useQuery(jobsQuery());
  const hosts = useQuery(hostsQuery());
  const [tab, setTab] = useState("overview");
  // The shell connects when its tab is first opened and stays up across tab changes.
  const [shellOpened, setShellOpened] = useState(false);
  if (query.isPending) return <Loading label="Stackbox 정보를 불러오는 중" />;
  if (query.error)
    return (
      <>
        <Link to="/" className="back-link">
          <ArrowLeft size={14} />
          Stackboxes
        </Link>
        <ErrorNotice
          error={query.error}
          retry={() => {
            void query.refetch();
          }}
        />
      </>
    );
  const stackbox = query.data;
  const history = [...(jobs.data ?? [])]
    .filter((j) => j.stackbox_id === id)
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
    ["failed", "broken"].includes(stackbox.status) &&
    !stackbox.active_job;
  const activeJob = history.find((j) => j.id === stackbox.active_job);
  // A remote stackbox keeps the agent digest it was created with; updates need a new stackbox.
  const host = hosts.data?.find((h) => h.id === stackbox.host_id);
  const staleAgent = Boolean(
    stackbox.agent_sha256 &&
    host?.agent?.artifact?.sha256 &&
    stackbox.agent_sha256 !== host.agent.artifact.sha256,
  );
  // A restored stackbox was deployed before freezing; its VM starts from disk without a deploy job.
  const deployed =
    history.some((j) => j.kind === "deploy" && j.status === "succeeded") ||
    Boolean(stackbox.snapshot_id);
  return (
    <>
      <Link to="/" className="back-link">
        <ArrowLeft size={14} />
        모든 stackbox
      </Link>
      <div className="detail-heading">
        <div className="detail-title">
          <span className="detail-avatar">
            <Boxes size={20} strokeWidth={1.6} />
          </span>
          <div>
            <h1>
              {stackbox.id}
              <StatusBadge status={stackboxState(stackbox, activeJob)} />
            </h1>
            <StackboxMeta stackbox={stackbox} host={host} />
          </div>
        </div>
        {stackbox.status !== "deleted" && (
          <div className="detail-actions">
            <DeployStackboxDialog stackbox={stackbox} lastJob={latestDeployment} />
            <StackboxActionsMenu
              stackbox={stackbox}
              deployed={deployed}
              lastExpansion={history.find((j) => j.kind === "expand")}
            />
          </div>
        )}
      </div>
      {!isDeployableStackbox(stackbox) && stackbox.status !== "deleted" && (
        <div className="warning-notice">
          기존 다중 VM 구성입니다. 이 Stackbox의 상태 조회와 삭제는 계속 사용할 수
          있습니다. 배포하려면 Stackbox를 새로 만드세요.
        </div>
      )}
      {staleAgent && (
        <div className="info-notice">
          <RefreshCw size={18} />
          <div>
            <strong>이 stackbox는 예전 Stackbox 실행기에 고정되어 있습니다.</strong>{" "}
            배포 진행 상황은 그 실행기가 보내는 단계까지만 표시됩니다. 새
            실행기를 쓰려면 같은 구성으로 stackbox를 새로 만드세요.
          </div>
        </div>
      )}
      {stackbox.observation_error && (
        <div role="alert" className="warning-notice">
          {stackbox.observation_error}
          <p className="mt-1 text-xs">
            마지막으로 확인한 VM 상태를 표시합니다. 메모리와 포트 예약은
            유지됩니다.
            {stackbox.last_seen_at &&
              ` · 마지막 확인 ${dateTime(stackbox.last_seen_at)}`}
          </p>
        </div>
      )}
      {stackbox.error && (
        <div className="error-notice">
          <CircleAlert size={18} className="shrink-0" />
          <div>
            <strong>
              {stackbox.status === "broken"
                ? "VM 상태를 확인하세요"
                : "마지막 작업을 완료하지 못했습니다"}
            </strong>
            <p>{stackbox.error}</p>
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
            VM이 부팅되면 public endpoint를 이 stackbox의 포트로 옮기고 서비스가
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
      {stackbox.status === "created" &&
        !stackbox.active_job &&
        isDeployableStackbox(stackbox) && (
          <div className="info-notice">
            <ShieldCheck size={18} />
            <div>
              <strong>VM이 준비되었습니다.</strong> OpenStack 배포를 시작하면
              API 서비스를 사용할 수 있습니다.
            </div>
          </div>
        )}
      {stackbox.status === "deleted" && (
        <div className="info-notice">
          <CheckCheck size={18} />
          <span>
            모든 VM이 삭제되었습니다. 작업 내역과 로그는 계속 확인할 수
            있습니다.
          </span>
        </div>
      )}
      <div className="detail-facts" role="group" aria-label="Stackbox 주요 정보">
        <div>
          <Cpu size={16} />
          <span>
            메모리<strong>{memory(stackboxMemory(stackbox))}</strong>
          </span>
        </div>
        <div>
          <Server size={16} />
          <span>
            가상 머신
            <strong>{runningVms(stackbox, activeJob)}</strong>
          </span>
        </div>
        <div>
          <HardDrive size={16} />
          <span>
            할당 디스크
            <strong>
              {stackbox.plan ? memory(stackbox.plan.disk_mib) : "기존 VM 구성"}
            </strong>
          </span>
        </div>
        <div>
          <Globe size={16} />
          <span>
            Public host
            <strong className="font-mono">
              {stackbox.exposure?.host ?? "내부 접속"}
            </strong>
          </span>
        </div>
        {stackbox.plan?.gpu && (
          <div>
            <CircuitBoard size={16} />
            <span>
              GPU
              <strong>
                {stackbox.plan.gpu.mode === "mtty" ? "mtty mdev" : "simulated"} ×
                {stackbox.plan.gpu.count}
                {stackbox.plan.gpu.mode === "simulated" &&
                (stackbox.plan.gpu.devices ?? 1) > 1
                  ? ` · 장치 ${stackbox.plan.gpu.devices}개`
                  : ""}
              </strong>
              {/* The device is advertised and reserved, never computed on. */}
              <span className="tag">가상 장치</span>
            </span>
          </div>
        )}
        {stackbox.plan?.pci_devices && stackbox.plan.pci_devices.length > 0 && (
          <PciPassthroughStat devices={stackbox.plan.pci_devices} />
        )}
      </div>
      <Tabs.Root
        value={tab}
        onValueChange={(value: string) => {
          setTab(value);
          if (value === "shell") setShellOpened(true);
        }}
        className="detail-tabs"
      >
        <Tabs.List className="tabs-list" aria-label="Stackbox 상세 메뉴">
          <Tabs.Tab value="overview" className="tab">
            Overview
          </Tabs.Tab>
          <Tabs.Tab value="logs" className="tab">
            Logs{stackbox.active_job && <span className="status-dot" />}
          </Tabs.Tab>
          <Tabs.Tab value="jobs" className="tab">
            Jobs<span className="count-pill">{history.length}</span>
          </Tabs.Tab>
          {stackbox.status !== "deleted" && stackbox.plan?.gpu && (
            <Tabs.Tab value="gpu" className="tab">
              GPU 부하
            </Tabs.Tab>
          )}
          <Tabs.Tab value="shell" className="tab">
            Shell
          </Tabs.Tab>
        </Tabs.List>
        <Tabs.Panel value="overview" className="tab-panel">
          <NodeTopology stackbox={stackbox} job={activeJob} />
          <div className="detail-bottom-grid">
            <DeploymentPhases stackbox={stackbox} />
            <Endpoints stackbox={stackbox} />
          </div>
          <RecentLogs
            id={id}
            live={Boolean(stackbox.active_job)}
            onOpenLogs={() => setTab("logs")}
          />
          <Datastores stackbox={stackbox} />
        </Tabs.Panel>
        <Tabs.Panel value="logs" className="tab-panel">
          <LogViewer id={id} />
        </Tabs.Panel>
        {stackbox.status !== "deleted" && stackbox.plan?.gpu && (
          <Tabs.Panel value="gpu" className="tab-panel">
            <GpuLoadPanel stackbox={stackbox} />
          </Tabs.Panel>
        )}
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
                description="이 stackbox에서 시작한 작업이 여기에 표시됩니다."
              />
            )}
          </section>
        </Tabs.Panel>
        <Tabs.Panel value="shell" className="tab-panel" keepMounted>
          {!shellTargets(stackbox.nodes).some(
            ({ node }) => node.status === "running",
          ) ? (
            <section className="panel">
              <EmptyState
                icon={<SquareTerminal size={24} />}
                title="VM이 실행 중이 아닙니다"
                description={
                  isAllInOneStackbox(stackbox)
                    ? "VM이 실행 중일 때 셸을 열 수 있습니다."
                    : isMultiNodeStackbox(stackbox)
                      ? "VM이 실행 중일 때 셸을 열 수 있습니다."
                      : "셸은 기존 다중 VM stackbox에서 지원하지 않습니다."
                }
              />
            </section>
          ) : (
            shellOpened && (
              <Suspense fallback={<Loading label="터미널을 불러오는 중" />}>
                <StackboxTerminal stackbox={stackbox} active={tab === "shell"} />
              </Suspense>
            )
          )}
        </Tabs.Panel>
      </Tabs.Root>
    </>
  );
}

/** One stackbox detail page renders one overflow menu, so a fixed id links trigger and menu. */
const MENU_ID = "stackbox-actions-menu";

/** The actions that are not the stackbox's one primary button, behind an overflow menu.
 * The menu stays mounted and only hides, so a dialog it opens outlives the menu closing. */
function StackboxActionsMenu({
  stackbox,
  deployed,
  lastExpansion,
}: {
  stackbox: Stackbox;
  deployed: boolean;
  lastExpansion?: Job;
}) {
  const [open, setOpen] = useState(false);
  const wrap = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    if (!open) return;
    const dismiss = (event: PointerEvent) => {
      if (!wrap.current?.contains(event.target as Node)) setOpen(false);
    };
    const escape = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      setOpen(false);
      // Closing hides the menu under the focused item, so hand focus back to the trigger.
      trigger.current?.focus();
    };
    document.addEventListener("pointerdown", dismiss);
    document.addEventListener("keydown", escape);
    return () => {
      document.removeEventListener("pointerdown", dismiss);
      document.removeEventListener("keydown", escape);
    };
  }, [open]);
  return (
    <div className="action-menu-wrap" ref={wrap}>
      <button
        ref={trigger}
        type="button"
        className="icon-button line"
        aria-label="더 많은 작업"
        aria-expanded={open}
        aria-controls={MENU_ID}
        title="배포 재개 · compute 추가 · 얼리기 · 삭제"
        onClick={() => setOpen((shown) => !shown)}
      >
        <MoreHorizontal size={16} />
      </button>
      <div
        id={MENU_ID}
        className="action-menu"
        role="group"
        aria-label="Stackbox 작업"
        hidden={!open}
        // A dialog opens from one of these buttons, so the menu steps aside.
        onClick={(event) => {
          if ((event.target as HTMLElement).closest("button")) setOpen(false);
        }}
      >
        <AddComputeDialog stackbox={stackbox} lastJob={lastExpansion} />
        <CheckServicesButton stackbox={stackbox} deployed={deployed} />
        <FreezeStackboxDialog stackbox={stackbox} />
        <DeleteStackboxDialog stackbox={stackbox} />
      </div>
    </div>
  );
}

/** The tail of the deploy log, so the overview answers "what is it doing" without a tab. */
export function RecentLogs({
  id,
  live,
  onOpenLogs,
}: {
  id: string;
  live: boolean;
  onOpenLogs: () => void;
}) {
  const logs = useQuery(logsQuery(id));
  // While a job runs, follow the same stream the logs tab uses; otherwise the tail is a snapshot.
  useLogStream(id, live && logs.isSuccess);
  const entries = (logs.data ?? []).slice(-6);
  return (
    <section className="panel recent-logs-panel" aria-label="최근 로그">
      <div className="panel-toolbar">
        <h2 className="flex items-center gap-2">
          <Terminal size={16} />
          최근 로그
          {entries.length > 0 && (
            <span className="count-pill">{entries.length}</span>
          )}
        </h2>
        <button type="button" className="text-link" onClick={onOpenLogs}>
          전체 로그
        </button>
      </div>
      {logs.isPending ? (
        <Loading compact label="로그를 불러오는 중" />
      ) : logs.error ? (
        <ErrorNotice
          error={logs.error}
          retry={() => {
            void logs.refetch();
          }}
        />
      ) : !entries.length ? (
        <p className="log-empty-inline">아직 기록된 로그가 없습니다.</p>
      ) : (
        <div
          className="log-body compact"
          role="log"
          aria-label="최근 배포 로그"
        >
          <LogLines entries={entries} />
        </div>
      )}
    </section>
  );
}

/** The facts under a stackbox's name: its owner, where its VMs run, the OpenStack release it
 * installs and when it was made. */
/** The host's own devices handed to compute-1 through VFIO, listed by their address. */
export function PciPassthroughStat({ devices }: { devices: string[] }) {
  return (
    <div>
      <Cpu size={16} />
      <span>
        PCI 패스스루
        <strong className="font-mono">{devices.join(", ")}</strong>
        <span className="tag">compute-1</span>
      </span>
    </div>
  );
}

export function StackboxMeta({ stackbox, host }: { stackbox: Stackbox; host?: RemoteHost }) {
  return (
    <p>
      {stackbox.owner || "담당자 미지정"}
      <span>·</span>
      {!stackbox.host_id
        ? "호스트 없음"
        : host?.kind === "local"
          ? `로컬 · ${stackbox.host_id}`
          : `원격 · ${stackbox.host_id}`}
      <span>·</span>
      {stackboxReleaseLabel(stackbox) ? <StackboxRelease stackbox={stackbox} /> : "릴리스 미기록"}
      <span>·</span>
      {dateTime(stackbox.created_at)} 생성
      {stackbox.snapshot_id && (
        <>
          <span>·</span>
          <Link to="/snapshots" className="hover:underline">
            스냅샷 {stackbox.snapshot_id}에서 복원
          </Link>
        </>
      )}
    </p>
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
function NodeFaults({ stackbox, node }: { stackbox: Stackbox; node: string }) {
  const client = useQueryClient();
  const mutation = useMutation({
    mutationFn: (impair: Impair) => api.setFaults(stackbox.id, node, impair),
    onSuccess: (updated) => {
      client.setQueryData(keys.stackbox(stackbox.id), updated);
      void client.invalidateQueries({ queryKey: keys.stackboxes });
    },
  });
  const current = faultLabel(stackbox.faults?.[node]);
  const usable = !["creating", "deleting", "deleted"].includes(stackbox.status);
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

/** One VM of the topology: its name, size, services, addresses and link faults. */
type VmCard = {
  key: string;
  hostname: string;
  role: string;
  memory: string;
  running: boolean;
  statusTitle: string;
  tags: string[];
  address: string;
  name: string;
  pending?: { host?: string };
  faults?: string;
};
function VmCardView({ stackbox, card }: { stackbox: Stackbox; card: VmCard }) {
  return (
    <article
      className={`topology-node ${card.running ? "node-running" : ""} ${card.pending ? "pending" : ""}`}
    >
      <div className="node-top">
        <Server size={16} strokeWidth={1.7} />
        <strong>{card.hostname}</strong>
        {card.role && <span className="node-role">{card.role}</span>}
        <span className="node-memory">{card.memory}</span>
        <span
          className={`node-status ${card.pending || card.running ? "running" : ""}`}
          title={card.pending ? "생성 중" : card.statusTitle}
          aria-hidden={card.pending ? true : undefined}
        />
      </div>
      {card.tags.length > 0 && (
        <div className="node-tags">
          {card.tags.map((tag) => (
            <span className="tag" key={tag}>
              {tag}
            </span>
          ))}
        </div>
      )}
      <div className="node-meta">
        <code>{card.address}</code>
        <span className="sep" aria-hidden="true">
          ·
        </span>
        <span className="node-name">{card.name}</span>
      </div>
      {card.pending ? (
        <div className="node-pending">
          <LoaderCircle size={15} className="animate-spin" />
          생성 중{card.pending.host && <span>· {card.pending.host}</span>}
        </div>
      ) : (
        card.faults && <NodeFaults stackbox={stackbox} node={card.faults} />
      )}
    </article>
  );
}

/** The host side of the topology: where the stackbox's public ports land. */
function TopologyHost({
  exposure,
  portRange,
  tenant,
}: {
  exposure: Stackbox["exposure"] | null;
  portRange: string;
  tenant: string;
}) {
  return (
    <div className="topology-host">
      <div className="topology-host-top">
        <Globe size={15} strokeWidth={1.7} />
        Host · link switch
      </div>
      <code>{exposure?.host ?? "public 포트 없음"}</code>
      <div className="topology-host-foot">
        <span className="status-dot" />
        {exposure ? "up · DNAT" : "노출 안 함"}
      </div>
      <div className="topology-net">
        <span>
          public <code>{portRange}</code>
        </span>
        <span>
          mgmt <code>10.254.0.0/24</code>
        </span>
        <span>
          tenant <code>{tenant}</code>
        </span>
      </div>
    </div>
  );
}

/** The public port block a stackbox's services answer on, as `:25000–25005`. It spans every
 * published service port, including Heat's CloudFormation and Skyline's console, and the
 * data stores. */
function publicPortRange(stackbox: Stackbox) {
  const exposure = stackbox.status !== "deleted" ? stackbox.exposure : null;
  if (!exposure) return "—";
  const services = stackboxPublicPorts(stackboxComponents(stackbox));
  if (!services.length) return "—";
  const ports = [
    ...services.map((service) => service.offset),
    ...dataStoreOffsets,
  ].map((offset) => exposure.port_base + offset);
  return ports.length === 1
    ? `:${ports[0]}`
    : `:${Math.min(...ports)}–${Math.max(...ports)}`;
}

/** A control VM and its computes on the stackbox's management and tenant links. */
function MultiNodeTopology({ stackbox, job }: { stackbox: Stackbox; job?: Job }) {
  const components = stackboxComponents(stackbox);
  const specs = stackbox.plan?.nodes ?? [];
  // A compute being added joins the drawing before Kolla lets the stackbox keep it.
  const pending = pendingCompute(stackbox, job);
  const exposure = stackbox.status !== "deleted" ? stackbox.exposure : null;
  const cards: VmCard[] = specs.map((spec) => {
    const node = stackbox.nodes.find(
      (n) => n.role === spec.role && (n.index ?? null) === (spec.index ?? null),
    );
    const suffix = nodeSuffix(spec);
    const control = spec.role === "control";
    return {
      key: suffix,
      hostname: nodeHostname(spec),
      role: control ? "control" : "compute",
      memory: memory(spec.memory_mib),
      running: node?.status === "running",
      statusTitle: node?.status ?? "준비되지 않음",
      tags: control
        ? [
            ...components.filter((c) => c !== "nova").map(componentLabel),
            "Nova API",
            "L3·DHCP",
            "Kolla",
          ]
        : ["nova-compute", "libvirt", "OVS agent"],
      address: nodeAddress(spec),
      name: node?.name ?? `${stackbox.id}-${suffix}`,
      faults: suffix,
    };
  });
  if (pending) {
    cards.push({
      key: nodeSuffix(pending),
      hostname: nodeHostname(pending),
      role: "compute",
      memory: memory(pending.memory_mib),
      running: false,
      statusTitle: "생성 중",
      tags: ["nova-compute", "libvirt", "OVS agent"],
      address: nodeAddress(pending),
      name: `${stackbox.id}-${nodeSuffix(pending)}`,
      pending: { host: pending.host },
    });
  }
  return (
    <section className="panel topology">
      <div className="panel-toolbar">
        <h2 className="flex items-center gap-2">
          <Network size={16} />
          VM topology
        </h2>
        <span className="topology-network">
          {`mgmt 10.254.0.0/24 · tenant VLAN · VM ${specs.length + (pending ? 1 : 0)}대`}
          {pending && " · 1대 생성 중"}
        </span>
      </div>
      <div className="topology-graph">
        <TopologyHost
          exposure={exposure}
          portRange={publicPortRange(stackbox)}
          tenant="VLAN"
        />
        <div className="topology-vms">
          {cards.map((card) => (
            <VmCardView stackbox={stackbox} card={card} key={card.key} />
          ))}
        </div>
      </div>
      <div className="topology-footer">
        <ShieldCheck size={15} />
        고립은 VM의 두 링크에서 모든 프레임을 버립니다. compute VM을 고립시키면
        Nova가 약 1분 뒤 down으로 표시하고, 그 뒤 evacuate를 시험할 수 있습니다.
      </div>
    </section>
  );
}
export function NodeTopology({ stackbox, job }: { stackbox: Stackbox; job?: Job }) {
  if (isMultiNodeStackbox(stackbox)) return <MultiNodeTopology stackbox={stackbox} job={job} />;
  const single = isAllInOneStackbox(stackbox);
  const components = stackboxComponents(stackbox);
  const sources = stackbox.plan
    ? stackbox.plan.nodes.map((spec) => ({
        role: spec.role,
        node: stackbox.nodes.find((node) => node.role === spec.role),
      }))
    : stackbox.nodes.map((node) => ({ role: node.role ?? "unknown", node }));
  const exposure = stackbox.status !== "deleted" ? stackbox.exposure : null;
  const cards: VmCard[] = sources.map(({ role, node }, index) => {
    const label = roleInfo[String(role)]?.label ?? String(role ?? "");
    const service =
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
          : (roleInfo[String(role)]?.service ?? "");
    // A single VM runs every chosen service inside itself; a legacy stackbox lists the roles it kept.
    const tags = single
      ? components.map(componentLabel)
      : service
        ? service.split(" · ").filter(Boolean)
        : [];
    return {
      key: node?.name ?? `${role}-${index}`,
      // A single VM keeps the fixed hostname every stackbox gives it; a legacy layout has none,
      // so its card is headed by the role it kept and named by the VM below.
      hostname: single ? nodeHostname({ role }) : label || "VM",
      role: single ? label : "",
      memory: node
        ? memory(node.memory_mib)
        : stackbox.status === "deleted"
          ? "삭제됨"
          : "생성 대기",
      running: node?.status === "running",
      statusTitle: node?.status ?? "준비되지 않음",
      tags,
      address: single ? "10.254.0.2" : (node?.ip ?? "—"),
      name: node?.name ?? `${stackbox.id}-${role}`,
    };
  });
  return (
    <section className="panel topology">
      <div className="panel-toolbar">
        <h2 className="flex items-center gap-2">
          <Network size={16} />
          VM topology
        </h2>
        <span className="topology-network">
          {single ? "stackbox0 10.254.0.2 · 단일 VM" : "기존 다중 VM 구성"}
        </span>
      </div>
      <div className="topology-graph">
        <TopologyHost
          exposure={exposure}
          portRange={publicPortRange(stackbox)}
          tenant={single ? "stackbox0" : "—"}
        />
        <div className="topology-vms">
          {cards.map((card) => (
            <VmCardView stackbox={stackbox} card={card} key={card.key} />
          ))}
        </div>
      </div>
      <div className="topology-footer">
        <ShieldCheck size={15} />
        {single
          ? "선택한 OpenStack 서비스와 기반 서비스를 이 VM 안에 설치합니다."
          : "기존 VM 구성을 표시합니다. 단일 VM 구성으로 자동 변환하지 않습니다."}
      </div>
    </section>
  );
}
export function DeploymentPhases({ stackbox }: { stackbox: Stackbox }) {
  return (
    <section className="panel phase-panel">
      <div className="panel-toolbar">
        <h2 className="flex items-center gap-2">
          <CheckCheck size={16} />
          배포 단계
        </h2>
        {stackbox.phase && <span className="chip mono">{stackbox.phase}</span>}
      </div>
      <ol className="phase-list">
        {phases.map((phase) => {
          // A snapshot stops the VM without undoing the deployment it finished.
          const ready = stackboxDeployed(stackbox);
          const current = stackbox.phase === phase.id;
          const running = current && Boolean(stackbox.active_job) && !ready;
          return (
            <li
              key={phase.id}
              className={ready ? "phase-done" : current ? "phase-current" : ""}
            >
              <span className="phase-marker">
                {ready ? <Check size={14} /> : current ? <span /> : null}
              </span>
              <div className="phase-text">
                <div className="phase-heading">
                  <strong>{phase.label}</strong>
                  <span>{phase.description}</span>
                </div>
                {running && stackbox.step && (
                  <span className="phase-step" title={stackbox.step}>
                    <LoaderCircle size={14} className="animate-spin" />
                    <span className="phase-step-text">{stackbox.step}</span>
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
/**
 * The stackbox's own relay exposure, read as its `state`. `enabled` only says the addresses answer now,
 * so it is the fallback for an answer that predates `state`.
 */
export function relayExposureState(
  exposure: RelayExposure | null | undefined,
): NonNullable<RelayExposure["state"]> {
  if (!exposure) return "off";
  const hasUrls = Object.keys(exposure.urls ?? {}).length > 0;
  if (exposure.state) {
    return exposure.state === "ready" && !hasUrls ? "off" : exposure.state;
  }
  return exposure.enabled && hasUrls ? "ready" : "off";
}

export function Endpoints({ stackbox }: { stackbox: Stackbox }) {
  const exposure = stackbox.status !== "deleted" ? stackbox.exposure : null;
  const deployed = stackboxDeployed(stackbox);
  // Credentials come from the VM, which a snapshot has stopped.
  const showLogin =
    stackbox.status === "ready" && stackboxComponents(stackbox).includes("horizon");
  // Ready stackboxes always read their endpoints: the relay exposure comes with them, Horizon or not.
  const login = useQuery({
    queryKey: keys.endpoints(stackbox.id),
    queryFn: () => api.endpoints(stackbox.id),
    enabled: stackbox.status === "ready",
    staleTime: 60_000,
    // The stackbox's own tunnel publishes its URLs once it is up; poll until it leaves `starting`.
    refetchInterval: (query) =>
      query.state.data?.relay_exposure?.state === "starting" ? 1500 : false,
  });
  const hosts = useQuery(hostsQuery());
  const host = hosts.data?.find((h) => h.id === stackbox.host_id);
  const enable = useEnableRelayExposure(stackbox.id);
  const disable = useDisableRelayExposure(stackbox.id);
  // Once the LAN view is picked it stays; turning the exposure on or off returns to the relay view.
  const [lanChosen, setLanChosen] = useState(false);
  const relayExposure = login.data?.relay_exposure;
  const relayState = relayExposureState(relayExposure);
  const relayUrls = relayExposure?.urls ?? {};
  const relayExposed = relayState === "ready";
  const viaRelay = relayExposed && !lanChosen;
  const lanServices = exposure
    ? stackboxPublicPorts(stackboxComponents(stackbox)).map((port) => ({
        key: port.id,
        name: port.label ?? componentLabel(port.component),
        type: port.type,
        url: `http://${exposure.host}:${exposure.port_base + port.offset}${port.path}`,
      }))
    : [];
  // Services are listed by the view in use: the relay's URL where it has one, the LAN URL otherwise.
  const services = lanServices.map((s) =>
    viaRelay && relayUrls[s.key] ? { ...s, url: relayUrls[s.key] } : s,
  );
  const turnOn = (rotate: boolean) =>
    enable.mutate(rotate, { onSuccess: () => setLanChosen(false) });
  const turnOff = () =>
    disable.mutate(undefined, { onSuccess: () => setLanChosen(false) });
  const busy = enable.isPending || disable.isPending;
  return (
    <section className="panel endpoints-panel">
      <div className="panel-toolbar">
        <h2 className="flex items-center gap-2">
          <Globe size={16} />
          Service endpoints
        </h2>
        {exposure && <span className="chip mono">{publicPortRange(stackbox)}</span>}
      </div>
      {stackbox.status === "ready" && services.length > 0 && (
        <div className="relay-exposure-bar">
          {host?.kind === "agent" ? (
            <p className="relay-exposure-note">
              다이얼인 에이전트 호스트는 릴레이 엔드포인트 프록시를 아직 지원하지 않습니다.
            </p>
          ) : login.isPending && !login.data ? null : relayState === "off" ? (
            <div className="relay-exposure-row">
              <p className="relay-exposure-note">
                이 Stackbox 전용 Cloudflare 터널을 열어 외부에서 OpenStack API·Horizon·DB에 접속합니다.
              </p>
              <Button type="button" disabled={busy} onClick={() => turnOn(false)}>
                외부 접속 켜기
              </Button>
            </div>
          ) : relayState === "starting" ? (
            <div className="relay-exposure-row">
              <p className="relay-exposure-status">
                <LoaderCircle size={15} className="animate-spin" />
                Cloudflare 터널 연결 중…
              </p>
              <Button type="button" disabled={busy} onClick={turnOff}>
                끄기
              </Button>
            </div>
          ) : relayState === "error" ? (
            <div className="relay-exposure-row">
              <p className="relay-exposure-note relay-exposure-error" role="alert">
                {relayExposure?.error || "Cloudflare 터널을 시작하지 못했습니다."}
              </p>
              <Button type="button" disabled={busy} onClick={() => turnOn(false)}>
                다시 시도
              </Button>
              <Button type="button" disabled={busy} onClick={turnOff}>
                끄기
              </Button>
            </div>
          ) : (
            <>
              <div className="relay-exposure-row">
                <div className="relay-view-toggle" role="group" aria-label="주소 보기">
                  <button
                    type="button"
                    aria-pressed={!viaRelay}
                    onClick={() => setLanChosen(true)}
                  >
                    LAN
                  </button>
                  <button
                    type="button"
                    aria-pressed={viaRelay}
                    onClick={() => setLanChosen(false)}
                  >
                    Relay
                  </button>
                </div>
                <Button type="button" disabled={busy} onClick={() => turnOn(true)}>
                  토큰 재발급
                </Button>
                <Button type="button" disabled={busy} onClick={turnOff}>
                  끄기
                </Button>
              </div>
              {viaRelay && (
                <p className="relay-exposure-note">
                  이 Stackbox 전용 Cloudflare 주소입니다. 다른 Stackbox 대시보드와 세션이 섞이지 않으며, 끄거나 토큰을 재발급하면 즉시 차단됩니다.
                </p>
              )}
            </>
          )}
          {(enable.error || disable.error) && (
            <ErrorNotice error={enable.error ?? disable.error} />
          )}
        </div>
      )}
      {!services.length ? (
        <div className="endpoint-note">
          {stackbox.status === "deleted"
            ? "환경이 삭제되어 서비스 주소를 사용할 수 없습니다."
            : "이 환경에는 public 포트가 없습니다. 내부 네트워크 주소로 접속하세요."}
        </div>
      ) : (
        <>
          <p className="field-hint ep-hint">
            {deployed
              ? "마지막 배포가 완료된 서비스 주소입니다."
              : "배포 완료 후 사용할 public 서비스 주소입니다."}
          </p>
          <div className="endpoint-list">
            {services.map((s) => (
              <div className="endpoint" key={s.key}>
                <div>
                  <span className="endpoint-icon">
                    {s.name === "Keystone" ? (
                      <ShieldCheck size={16} />
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
      {stackbox.expires_at && (
        <div className="expiry-note">
          만료 시각: {dateTime(stackbox.expires_at)} · 자동 삭제 없음
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
 * MariaDB and RabbitMQ ship with every stackbox; integrations connect through their published ports.
 * Shares the endpoints query cache with the service panel above.
 */
export function Datastores({ stackbox }: { stackbox: Stackbox }) {
  const deployed = stackboxDeployed(stackbox);
  const query = useQuery({
    queryKey: keys.endpoints(stackbox.id),
    queryFn: () => api.endpoints(stackbox.id),
    enabled: stackbox.status === "ready",
    staleTime: 60_000,
  });
  const stores = query.data?.datastores ?? [];
  const relayUrls = query.data?.relay_exposure?.urls ?? {};
  const relayExposed = relayExposureState(query.data?.relay_exposure) === "ready";
  // Relay is the default once exposed; picking LAN sticks for this view.
  const [lanChosen, setLanChosen] = useState(false);
  const viaRelay = relayExposed && !lanChosen;
  const tunnelScript = relayUrls["tunnel-script"];
  // Zero-install one-liner: Node 22+ opens MariaDB 3306 and AMQP 5672 locally.
  const tunnelCommand =
    viaRelay && tunnelScript
      ? `curl -fsSL ${tunnelScript} | node --input-type=module`
      : undefined;
  /** Relay view swaps the address (and console link) per store; LAN view keeps the published ports. */
  const relayOverride = (id: string): { addressOverride?: string; hrefOverride?: string } => {
    if (!viaRelay) return {};
    if (id === "rabbitmq-management") {
      const url = relayUrls["rabbitmq-management"];
      return url ? { addressOverride: url, hrefOverride: url } : {};
    }
    if (!tunnelScript) return {};
    if (id === "mariadb") return { addressOverride: "127.0.0.1:3306" };
    if (id === "rabbitmq") return { addressOverride: "127.0.0.1:5672" };
    return {};
  };
  return (
    <section className="panel datastore-panel">
      <div className="panel-toolbar">
        <h2 className="flex items-center gap-2">
          <Database size={16} />
          Data stores
        </h2>
        <div className="datastore-toolbar-actions">
          {relayExposed && (
            <div className="relay-view-toggle" role="group" aria-label="데이터 스토어 주소 보기">
              <button
                type="button"
                aria-pressed={!viaRelay}
                onClick={() => setLanChosen(true)}
              >
                LAN
              </button>
              <button
                type="button"
                aria-pressed={viaRelay}
                onClick={() => setLanChosen(false)}
              >
                Relay
              </button>
            </div>
          )}
          {deployed && stores.length > 0 && (
            <span className="chip mono">{stores.length}</span>
          )}
        </div>
      </div>
      <div className="panel-body">
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
            이 stackbox는 데이터 스토어 포트를 공개하지 않았습니다. 새로 만든
            stackbox부터 MariaDB·RabbitMQ 포트가 함께 열립니다.
          </div>
        ) : (
          <>
            {tunnelCommand && (
              <div className="tunnel-bar">
                <div className="tunnel-bar-command">
                  <code>{tunnelCommand}</code>
                  <CopyText value={tunnelCommand} hideValue label="터널 명령" />
                </div>
                <p className="tunnel-bar-note">
                  stackbox 설치 없이 Node 22+로 MariaDB(127.0.0.1:3306)와 RabbitMQ
                  AMQP(127.0.0.1:5672)를 한 번에 엽니다. (포트 변경: MYSQL_PORT=13306
                  AMQP_PORT=15672)
                </p>
              </div>
            )}
            <p className="field-hint">
              {viaRelay
                ? "Cloudflare 릴레이 주소입니다. 관리 UI는 브라우저에서 바로 열고, DB·AMQP는 위 터널 명령으로 로컬 포트에 연결합니다."
                : "stackbox마다 같은 VM 안에서 실행되는 데이터베이스와 메시지 큐입니다. 포트는 " +
                  `${stackbox.exposure?.port_base ?? "—"}번대 블록의 해당 offset을 씁니다.`}
            </p>
            <div className="datastore-list">
              {stores.map((store) => (
                <DatastoreCard
                  store={store}
                  key={store.id}
                  {...relayOverride(store.id)}
                />
              ))}
            </div>
            {!viaRelay && (
              <div className="credential-note">
                <ShieldCheck size={16} />
                <p>
                  이 포트는 stackbox VM의 고정 주소로 DNAT됩니다. 외부에 공개한
                  포털이라면 인증정보 표시를 끄고 방화벽으로 3306·5672·15672를
                  제한하세요.
                </p>
              </div>
            )}
          </>
        )}
      </div>
    </section>
  );
}

/** One data store: connection, account, and the schemas a client may open. */
export function DatastoreCard({
  store,
  addressOverride,
  hrefOverride,
}: {
  store: Datastore;
  /** Relay view: the address shown and copied instead of host:port. */
  addressOverride?: string;
  /** Relay view: the console link target. */
  hrefOverride?: string;
}) {
  const [revealed, setRevealed] = useState(false);
  const console_ = store.protocol === "http";
  const relay = addressOverride !== undefined || hrefOverride !== undefined;
  const address = addressOverride ?? `${store.host}:${store.port}`;
  const copyValue = addressOverride ?? store.url;
  return (
    <article className={relay ? "datastore relay-datastore" : "datastore"}>
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
            href={hrefOverride ?? store.url}
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
          <KeyRound size={16} />
          <code>{address}</code>
          <CopyText value={copyValue} hideValue label="URL" />
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
            {shown ? <EyeOff size={16} /> : <Eye size={16} />}
          </button>
        )}
        <CopyText value={value} hideValue label={label} />
      </dd>
    </div>
  );
}

function LayersIcon() {
  return <Boxes size={16} />;
}
