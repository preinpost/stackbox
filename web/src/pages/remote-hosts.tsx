import { Checkbox } from "@base-ui/react/checkbox";
import { Dialog } from "@base-ui/react/dialog";
import { Input } from "@base-ui/react/input";
import { Radio } from "@base-ui/react/radio";
import { RadioGroup } from "@base-ui/react/radio-group";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  Check,
  CircleAlert,
  Fingerprint,
  KeyRound,
  LoaderCircle,
  Monitor,
  Plus,
  RadioTower,
  RefreshCw,
  Upload,
  Server,
  ShieldCheck,
  Terminal,
  Trash2,
} from "lucide-react";
import { useRef, useState } from "react";
import {
  api,
  farmsQuery,
  hostsQuery,
  hostJobsQuery,
  keys,
  hostQuery,
  snapshotsQuery,
  type HostCapacity,
  type HostJob,
  type RemoteHost,
} from "../api/client";
import { CreateFarmDialog } from "../components/farm-actions";
import {
  Button,
  CopyText,
  ErrorNotice,
  Loading,
  Modal,
  PageHeading,
  StatusBadge,
} from "../components/ui";
import {
  agentBuildMismatch,
  dateTime,
  hostNotice,
  hostReady,
  memory,
  platform,
  remoteAgentNeedsUpdate,
} from "../lib";

export function RemoteHostsPage() {
  const hosts = useQuery(hostsQuery());
  const jobs = useQuery(hostJobsQuery());
  const coordinator = useQuery(hostQuery());
  const farms = useQuery(farmsQuery());
  const snapshots = useQuery(snapshotsQuery());
  const client = useQueryClient();
  const buildId = coordinator.data?.build_id;
  // Unset selects the first host.
  const [selectedId, setSelectedId] = useState<string>();
  const [open, setOpen] = useState(false);
  const [editing, setEditing] = useState<RemoteHost>();
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<unknown>();
  const selected =
    hosts.data?.find((h) => h.id === selectedId) ?? hosts.data?.[0];
  const local = selected?.kind === "local";
  // Only SSH hosts have credentials, a host key and an installed agent.
  const ssh = selected?.kind === "ssh";
  const localRegistered = hosts.data?.some((h) => h.kind === "local");
  const farmCount = (hostId: string) =>
    farms.data?.filter((f) => f.status !== "deleted" && f.host_id === hostId)
      .length ?? 0;
  const refresh = async () => {
    await Promise.all([
      client.invalidateQueries({ queryKey: keys.hosts }),
      client.invalidateQueries({ queryKey: keys.hostJobs }),
      client.invalidateQueries({ queryKey: keys.host }),
    ]);
  };
  const action = useMutation({
    mutationFn: ({
      id,
      kind,
      snapshots = false,
    }: {
      id: string;
      kind: "check" | "prepare" | "remove" | "purge" | "agent";
      snapshots?: boolean;
    }) =>
      kind === "check"
        ? api.checkHost(id)
        : kind === "prepare"
          ? api.prepareHost(id)
          : kind === "agent"
            ? api.installHostAgent(id)
            : kind === "purge"
              ? api.purgeHost(id, snapshots)
              : api.unregisterHost(id),
    onSuccess: refresh,
  });
  // Registering this machine checks it right away, like a new remote host.
  const registerLocal = useMutation({
    mutationFn: async () => {
      const host = await api.registerLocalHost();
      setSelectedId(host.id);
      await api.checkHost(host.id);
    },
    onSettled: refresh,
  });
  const agentMismatch = agentBuildMismatch(selected?.agent?.artifact, buildId);
  const hostJobs =
    jobs.data
      ?.filter((j) => j.host_id === selected?.id)
      .sort(
        (a, b) => b.created_at - a.created_at || b.id.localeCompare(a.id),
      ) ?? [];
  return (
    <>
      <PageHeading
        eyebrow="INFRASTRUCTURE"
        title="Hosts"
        description="Farm을 실행할 호스트를 등록하고 준비하세요. 로컬 호스트와 원격 서버를 같은 방식으로 관리합니다."
        actions={
          <>
            <Button onClick={() => void refresh()} disabled={hosts.isFetching}>
              <RefreshCw size={15} />
              새로고침
            </Button>
            {hosts.isSuccess && !localRegistered && (
              <Button
                disabled={registerLocal.isPending}
                onClick={() => registerLocal.mutate()}
              >
                {registerLocal.isPending ? (
                  <LoaderCircle size={15} className="animate-spin" />
                ) : (
                  <Monitor size={15} />
                )}
                로컬 호스트 등록
              </Button>
            )}
            <RegisterAgentHostDialog
              onRegistered={async (id) => {
                setSelectedId(id);
                await refresh();
              }}
            />
            <Dialog.Root
              open={open}
              onOpenChange={(next) => {
                if (!pending) setOpen(next);
              }}
            >
              <Dialog.Trigger
                render={<Button variant="primary" />}
                onClick={() => setEditing(undefined)}
              >
                <Plus size={16} />
                원격 호스트 등록
              </Dialog.Trigger>
              {open && (
                <RegisterHostForm
                  host={editing}
                  onPending={setPending}
                  close={() => setOpen(false)}
                  onRegistered={async (host) => {
                    setSelectedId(host.id);
                    setError(undefined);
                    try {
                      await api.checkHost(host.id);
                    } catch (e) {
                      setError(e);
                    }
                    await refresh();
                  }}
                />
              )}
            </Dialog.Root>
          </>
        }
      />
      <div className="remote-workflow">
        <ShieldCheck size={19} />
        <p>
          <strong>호스트 등록 → 점검 → 환경 준비 → Farm 만들기</strong>
          <span>
            Farm은 등록하고 준비를 마친 호스트에서만 실행됩니다. 로컬 호스트는
            등록 후 바로 점검하고, 원격 서버는 SSH 점검과 환경 준비, Farm 실행기
            설치를 거칩니다. Farm마다 VM 한 대를 선택한 호스트에서 실행합니다.
          </span>
        </p>
      </div>
      {(hosts.error ||
        jobs.error ||
        action.error ||
        registerLocal.error ||
        error) && (
        <ErrorNotice
          error={
            hosts.error ||
            jobs.error ||
            action.error ||
            registerLocal.error ||
            error
          }
        />
      )}
      <div className="remote-host-grid">
        <section className="panel remote-host-list" aria-label="호스트 목록">
          <div className="panel-toolbar">
            <h2>
              호스트 <span className="muted">{hosts.data?.length ?? 0}</span>
            </h2>
            <span className="muted text-xs">
              {hosts.data?.filter((h) => hostReady(h, buildId)).length ?? 0}{" "}
              사용 가능
            </span>
          </div>
          {hosts.isPending ? (
            <Loading />
          ) : (
            hosts.data?.map((host) => (
              <button
                key={host.id}
                onClick={() => setSelectedId(host.id)}
                className={`remote-host-row ${selected?.id === host.id ? "selected" : ""}`}
                aria-pressed={selected?.id === host.id}
              >
                <span className="remote-server-icon">
                  {host.kind === "local" ? (
                    <Monitor size={20} strokeWidth={1.5} />
                  ) : host.kind === "agent" ? (
                    <RadioTower size={20} strokeWidth={1.5} />
                  ) : (
                    <Server size={20} strokeWidth={1.5} />
                  )}
                </span>
                <span className="min-w-0 flex-1">
                  <strong>{host.id}</strong>
                  <code>
                    {host.kind === "local"
                      ? "로컬"
                      : host.kind === "agent"
                        ? host.connected
                          ? "에이전트 연결됨"
                          : "에이전트 연결 끊김"
                        : `${host.address}:${host.port}`}
                    {host.inspection &&
                      ` · ${platform(host.inspection.os, host.inspection.arch)}`}
                    {` · Farm ${farmCount(host.id)}개`}
                  </code>
                  {remoteAgentNeedsUpdate(host) ? (
                    <span className="field-hint">실행기 업데이트 필요</span>
                  ) : (
                    <StatusBadge status={host.status} />
                  )}
                </span>
              </button>
            ))
          )}
          {hosts.isSuccess && !hosts.data.length && (
            <p className="remote-unchecked">
              Farm을 실행할 호스트가 없습니다. coordinator가 실행 중인 머신에서
              실행하려면{" "}
              <button
                className="text-link"
                disabled={registerLocal.isPending}
                onClick={() => registerLocal.mutate()}
              >
                로컬 호스트 등록
              </button>
              , 다른 서버에서 실행하려면{" "}
              <button className="text-link" onClick={() => setOpen(true)}>
                원격 호스트 등록
              </button>
              을 선택하세요.
            </p>
          )}
        </section>
        {selected && (
          <section
            className="panel remote-host-detail"
            aria-label={`${selected.id} 호스트 상세`}
          >
            <div className="remote-detail-heading">
              <div>
                <p className="eyebrow">
                  {local ? "LOCAL HOST" : ssh ? "REMOTE HOST" : "AGENT HOST"}
                </p>
                <h2>{selected.id}</h2>
                <p className="muted text-xs mt-1 break-all">
                  {local
                    ? "localhost · coordinator(farmd)가 실행 중인 머신"
                    : ssh
                      ? `${selected.username}@${selected.address}:${selected.port}`
                      : selected.connected
                        ? "에이전트 연결됨 · 호스트가 coordinator로 접속해 있습니다"
                        : "에이전트 연결 끊김 · 호스트에서 farm agent connect를 실행하세요"}
                </p>
              </div>
              <StatusBadge status={selected.status} />
            </div>
            {selected.error && (
              <div role="alert" className="remote-error">
                <CircleAlert size={16} />
                <p>{selected.error}</p>
              </div>
            )}
            {ssh && (
              <div className="remote-fingerprint">
                <Fingerprint size={17} />
                <div>
                  <span>
                    등록된 서버 지문 ·{" "}
                    {selected.auth_method === "password"
                      ? "비밀번호 인증"
                      : "SSH 키 인증"}
                  </span>
                  <code>{selected.fingerprint}</code>
                </div>
              </div>
            )}
            {selected.inspection ? (
              <HostInspection
                host={selected}
                farms={farmCount(selected.id)}
                coordinatorBuildId={buildId}
              />
            ) : (
              <p className="remote-unchecked">
                {ssh
                  ? "아직 점검하지 않았습니다. SSH 접속과 실행 조건을 확인하세요."
                  : "아직 점검하지 않았습니다. 호스트의 msb·가상화·golden image를 확인하세요."}
              </p>
            )}
            <div className="remote-actions">
              <CreateFarmDialog
                key={`${selected.id}-farm`}
                hostId={selected.id}
                label="이 호스트에 Farm 만들기"
                disabled={!hostReady(selected, buildId)}
              />
              {ssh && (
                <Button
                  disabled={!!selected.active_job || action.isPending}
                  onClick={() => {
                    setEditing(selected);
                    setOpen(true);
                  }}
                >
                  <KeyRound size={14} />
                  인증 변경
                </Button>
              )}
              {ssh && (
                <Button
                  variant={agentMismatch ? "primary" : "default"}
                  disabled={
                    !!selected.active_job ||
                    action.isPending ||
                    !coordinator.data?.agent_artifacts?.some(
                      (artifact) => artifact.arch === selected.inspection?.arch,
                    )
                  }
                  onClick={() =>
                    action.mutate({ id: selected.id, kind: "agent" })
                  }
                  title={
                    agentMismatch
                      ? "설치된 실행기의 빌드가 coordinator와 다릅니다. 재설치하세요."
                      : undefined
                  }
                >
                  <Terminal size={14} />
                  {agentMismatch
                    ? "Farm 실행기 재설치"
                    : remoteAgentNeedsUpdate(selected)
                      ? "Farm 실행기 업데이트"
                      : "Farm 실행기 설치"}
                </Button>
              )}
              <Button
                disabled={!!selected.active_job || action.isPending}
                onClick={() =>
                  action.mutate({ id: selected.id, kind: "check" })
                }
              >
                <RefreshCw size={14} />
                {ssh ? "SSH 점검" : "점검"}
              </Button>
              <HostActionDialog
                key={`${selected.id}-prepare`}
                host={selected}
                kind="prepare"
                disabled={!!selected.active_job || action.isPending}
                farms={farmCount(selected.id)}
                run={() =>
                  action.mutateAsync({ id: selected.id, kind: "prepare" })
                }
              />
              <HostActionDialog
                key={`${selected.id}-remove`}
                host={selected}
                kind="remove"
                disabled={!!selected.active_job || action.isPending}
                farms={farmCount(selected.id)}
                snapshots={local ? (snapshots.data?.length ?? 0) : 0}
                coordinatorBuildId={buildId}
                run={(purge, deleteSnapshots) =>
                  action.mutateAsync({
                    id: selected.id,
                    kind: purge ? "purge" : "remove",
                    snapshots: deleteSnapshots,
                  })
                }
              />
            </div>
            {!hostReady(selected, buildId) && !selected.active_job && (
              <p className="field-hint mt-3">{hostNotice(selected, buildId)}</p>
            )}
            {ssh &&
              selected.inspection &&
              !coordinator.data?.agent_artifacts?.some(
                (artifact) => artifact.arch === selected.inspection?.arch,
              ) && (
                <p className="field-hint mt-3">
                  {selected.inspection.arch} 실행기가 없습니다.
                  scripts/build-agent.sh {selected.inspection.arch}로 빌드한 뒤
                  Coordinator를 재시작하세요.
                </p>
              )}
            {selected.active_job && (
              <div role="status" className="remote-progress">
                <LoaderCircle size={15} className="animate-spin" />
                {hostJobs
                  .find((j) => j.id === selected.active_job)
                  ?.events.at(-1)?.message ?? "작업 대기 중"}
              </div>
            )}
          </section>
        )}
      </div>
      {selected && (
        <section className="panel mt-5">
          <div className="panel-toolbar">
            <h2 className="flex items-center gap-2">
              <Terminal size={16} />
              {selected.id} 작업 이력
            </h2>
            <span className="muted text-xs">최근 10개</span>
          </div>
          {jobs.isPending ? (
            <Loading />
          ) : !hostJobs.length ? (
            <p className="remote-unchecked">
              점검·준비 작업의 진행 단계와 결과가 여기에 표시됩니다.
            </p>
          ) : (
            hostJobs
              .slice(0, 10)
              .map((job) => <HostJobHistory job={job} key={job.id} />)
          )}
        </section>
      )}
    </>
  );
}

/** A host whose agent dials in. Its token is shown once, with the command to run there. */
export function RegisterAgentHostDialog({
  onRegistered,
}: {
  onRegistered: (id: string) => Promise<void>;
}) {
  const [open, setOpen] = useState(false);
  const [id, setId] = useState("");
  const register = useMutation({
    mutationFn: () => api.registerAgentHost(id),
    onSuccess: (registered) => onRegistered(registered.host.id),
  });
  const token = register.data?.token;
  const command = token
    ? `FARM_AGENT_TOKEN=${token} farm agent connect <coordinator URL>`
    : "";
  return (
    <Dialog.Root
      open={open}
      onOpenChange={(next) => {
        if (register.isPending) return;
        setOpen(next);
        if (!next) {
          // The token is not shown again once the dialog closes.
          register.reset();
          setId("");
        }
      }}
    >
      <Dialog.Trigger render={<Button />}>
        <RadioTower size={15} />
        에이전트 호스트 등록
      </Dialog.Trigger>
      {open && (
        <Modal
          title={token ? `${id} 에이전트 연결` : "에이전트 호스트 등록"}
          description={
            token
              ? "호스트에서 아래 명령을 실행하면 coordinator로 접속합니다. 토큰은 지금만 표시됩니다."
              : "호스트의 에이전트가 coordinator로 직접 접속합니다. SSH나 열린 포트가 필요 없어 NAT 뒤의 머신이나 클라우드에 둔 coordinator에도 연결할 수 있습니다."
          }
          closeDisabled={register.isPending}
        >
          {token ? (
            <div className="modal-body space-y-4">
              <label className="field">
                <span>실행할 명령</span>
                <code className="agent-command">{command}</code>
                <CopyText value={command} hideValue name="실행 명령" />
              </label>
              <p className="field-hint">
                <code>&lt;coordinator URL&gt;</code>에는 호스트에서 접근할 수
                있는 farmd 주소(예: <code>https://farm.example</code>)를
                넣으세요. 호스트에는 같은 빌드의 <code>farm</code>
                (Linux 서버는 <code>farm-agent connect</code>)이 필요합니다.
                연결되면 이 호스트를 점검하고 환경 준비를 실행하세요.
              </p>
              <div className="modal-footer justify-end">
                <Dialog.Close render={<Button variant="primary" />}>
                  완료
                </Dialog.Close>
              </div>
            </div>
          ) : (
            <form
              onSubmit={(event) => {
                event.preventDefault();
                register.mutate();
              }}
            >
              <div className="modal-body space-y-4">
                <label className="field">
                  <span>호스트 ID</span>
                  <Input
                    autoFocus
                    value={id}
                    onValueChange={setId}
                    className="input font-mono"
                    placeholder="edge-01"
                    required
                    maxLength={48}
                    pattern="[a-z0-9]([a-z0-9-]{0,46}[a-z0-9])?"
                    disabled={register.isPending}
                  />
                </label>
                {register.error && <ErrorNotice error={register.error} />}
              </div>
              <div className="modal-footer justify-end">
                <Dialog.Close render={<Button disabled={register.isPending} />}>
                  취소
                </Dialog.Close>
                <Button
                  type="submit"
                  variant="primary"
                  disabled={register.isPending || !id}
                >
                  {register.isPending && (
                    <LoaderCircle size={14} className="animate-spin" />
                  )}
                  등록하고 토큰 받기
                </Button>
              </div>
            </form>
          )}
        </Modal>
      )}
    </Dialog.Root>
  );
}

/** Shared by local and remote hosts so both show the same resource facts. */
function HostResources({
  resources: r,
  capacity,
  farms,
}: {
  resources: {
    os: string;
    arch: string;
    memory_available_mib: number;
    memory_total_mib: number;
    disk_available_mib?: number | null;
  };
  capacity?: HostCapacity | null;
  farms: number;
}) {
  return (
    <>
      <div className="remote-resources">
        <div>
          <span>Platform</span>
          <strong>{platform(r.os, r.arch)}</strong>
        </div>
        <div>
          <span>Memory · 여유 / 전체</span>
          <strong>
            {memory(r.memory_available_mib)} / {memory(r.memory_total_mib)}
          </strong>
        </div>
        <div>
          <span>Disk · 여유 공간</span>
          <strong>
            {r.disk_available_mib == null ? "—" : memory(r.disk_available_mib)}
          </strong>
        </div>
      </div>
      <p className="field-hint mb-3">
        Farm {farms}개
        {capacity &&
          ` · 메모리 예약 ${memory(capacity.memory_reserved_mib)} / ${memory(capacity.memory_budget_mib)} · 추가 예약 가능 ${memory(Math.max(0, capacity.memory_budget_mib - capacity.memory_reserved_mib))}`}
      </p>
    </>
  );
}

function HostInspection({
  host,
  farms,
  coordinatorBuildId,
}: {
  host: RemoteHost;
  farms: number;
  coordinatorBuildId?: string;
}) {
  const i = host.inspection!;
  // This machine and dial-in hosts run their own agent and prepare their image by hand.
  const local = host.kind !== "ssh";
  const runtime = [
    {
      label: local
        ? "macOS · Linux, x86_64 · aarch64"
        : "Linux x86_64 · aarch64",
      ready: i.compatible,
    },
    {
      label: i.os === "macos" ? "HVF 가상화" : "KVM 접근",
      ready: i.kvm_available,
    },
    { label: "msb 0.7.8", ready: i.msb_compatible },
    // A hand-prepared golden image needs no Docker access from the agent.
    ...(local ? [] : [{ label: "Docker 접근", ready: i.docker_available }]),
    { label: "Golden image", ready: i.golden_image_available },
    { label: "VM 부팅·실행 검증", ready: i.vm_boot_verified },
  ];
  // Only SSH hosts get an agent installed by the coordinator.
  const agent = [
    {
      label: remoteAgentNeedsUpdate(host)
        ? "Farm 실행기 · 업데이트 필요"
        : "단일 VM Farm 실행기",
      ready: host.agent?.artifact?.supports_single_vm === true,
    },
    {
      label: agentBuildMismatch(host.agent?.artifact, coordinatorBuildId)
        ? "Farm 실행기 버전 · 재설치 필요"
        : "Farm 실행기 버전",
      ready:
        Boolean(host.agent) &&
        !agentBuildMismatch(host.agent?.artifact, coordinatorBuildId),
    },
  ];
  const checks = local ? runtime : [...runtime, ...agent];
  return (
    <>
      <HostResources resources={i} capacity={host.capacity} farms={farms} />
      <ul className="remote-checks">
        {checks.map((check) => (
          <li
            key={check.label}
            className={check.ready ? "check-ok" : "check-missing"}
          >
            {check.ready ? <Check size={15} /> : <CircleAlert size={15} />}
            <span>{check.label}</span>
            <span>{check.ready ? "확인됨" : "확인 필요"}</span>
          </li>
        ))}
      </ul>
      <dl className="build-compare">
        <div>
          <dt>msb</dt>
          <dd className={!i.msb_version ? "build-unknown" : ""}>
            {i.msb_version ?? "미설치"}
          </dd>
        </div>
        <div>
          <dt>coordinator</dt>
          <dd className={!coordinatorBuildId ? "build-unknown" : ""}>
            {coordinatorBuildId || "미상"}
          </dd>
        </div>
        {!local && (
          <div>
            <dt>remote</dt>
            <dd
              className={
                agentBuildMismatch(host.agent?.artifact, coordinatorBuildId)
                  ? "build-mismatch"
                  : !host.agent
                    ? "build-unknown"
                    : ""
              }
            >
              {host.agent?.artifact?.build_id || "설치 안 됨"}
            </dd>
          </div>
        )}
      </dl>
      <p className="field-hint">
        최근 점검 {dateTime(i.checked_at)}
        {!i.kvm_available &&
          " · VM 위에서 실행한다면 nested virtualization을 먼저 활성화하세요."}
      </p>
    </>
  );
}

export function HostJobHistory({ job }: { job: HostJob }) {
  return (
    <details
      className="host-job-history"
      open={job.status === "running" || job.status === "queued"}
    >
      <summary>
        <strong>{job.host_id}</strong>
        <span>
          {job.kind === "check"
            ? "점검"
            : job.kind === "install_agent"
              ? "Farm 실행기 설치"
              : "환경 준비"}
        </span>
        <StatusBadge status={job.status} />
        <time>{dateTime(job.created_at)}</time>
      </summary>
      {job.events.length > 0 && (
        <ol>
          {job.events.map((event, index) => (
            <li key={index}>
              <time>{dateTime(event.at)}</time>
              <span>{event.message}</span>
            </li>
          ))}
        </ol>
      )}
      {job.error && <p className="job-error">{job.error}</p>}
      {!job.events.length && (
        <p className="field-hint px-5 pb-4">실행 대기 중입니다.</p>
      )}
    </details>
  );
}

function HostActionDialog({
  host,
  kind,
  disabled,
  farms,
  snapshots = 0,
  coordinatorBuildId,
  run,
}: {
  host: RemoteHost;
  kind: "prepare" | "remove";
  disabled: boolean;
  farms: number;
  /** Snapshots kept on this host; only the local host keeps any. */
  snapshots?: number;
  coordinatorBuildId?: string;
  run: (purge: boolean, deleteSnapshots: boolean) => Promise<unknown>;
}) {
  const [open, setOpen] = useState(false);
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<unknown>();
  const prepare = kind === "prepare";
  // The local and dial-in hosts run their own agent and prepare msb by hand.
  const local = host.kind !== "ssh";
  const place = host.kind === "local" ? "로컬 호스트" : "이 호스트";
  const [purge, setPurge] = useState(true);
  // Off by default: a snapshot that was never exported cannot be recovered.
  const [deleteSnapshots, setDeleteSnapshots] = useState(false);
  const mismatch =
    !prepare && agentBuildMismatch(host.agent?.artifact, coordinatorBuildId);
  // A mismatched agent cannot clean up, so the disabled box also means records only.
  const purging = purge && !mismatch;
  return (
    <Dialog.Root
      open={open}
      onOpenChange={(next) => {
        if (!pending) setOpen(next);
      }}
    >
      <Dialog.Trigger
        render={
          <Button variant={prepare ? "primary" : "ghost"} disabled={disabled} />
        }
      >
        {prepare ? <Terminal size={14} /> : <Trash2 size={14} />}
        {prepare ? "환경 준비" : "등록 해제"}
      </Dialog.Trigger>
      {open && (
        <Modal
          title={`${host.id} ${prepare ? "환경 준비" : "등록 해제"}`}
          description={
            prepare
              ? local
                ? `${place}에서 다음 작업을 실행합니다.`
                : "이 서버에서 다음 작업을 실행합니다."
              : local
                ? `Coordinator에 저장된 ${place}의 호스트 정보를 삭제합니다. 필요하면 ${place}의 farm VM과 디스크도 함께 정리합니다.`
                : "Coordinator에 저장된 호스트 정보와 인증 정보를 삭제합니다. 필요하면 이 호스트의 VM과 디스크도 함께 정리합니다."
          }
          closeDisabled={pending}
        >
          <div className="modal-body space-y-4">
            {prepare && local ? (
              <>
                <ol className="remote-prepare-list">
                  <li>msb 0.7.8·가상화·golden image 확인</li>
                  <li>512 MiB 시험 VM 부팅 → 명령 실행 → 시험 VM 삭제</li>
                </ol>
                <p className="field-hint">
                  {place}에는 msb 0.7.8과 golden image를 직접 준비합니다.
                  이미지가 없다면 먼저 <code>task docker-build</code>를
                  실행하세요.
                </p>
              </>
            ) : prepare ? (
              <>
                <ol className="remote-prepare-list">
                  <li>msb 0.7.8과 libkrunfw 설치 또는 기존 버전 확인</li>
                  <li>golden image가 없으면 Docker로 빌드하고 msb에 로드</li>
                  <li>512 MiB 시험 VM 부팅 → 명령 실행 → 시험 VM 삭제</li>
                </ol>
                <p className="field-hint">
                  Linux, 동일 CPU 아키텍처, /dev/kvm 접근, 가용 메모리 768 MiB와
                  디스크 8 GiB 이상이 필요합니다. 이미지 빌드에는 Docker 접근
                  권한이 필요하며 최대 20분 정도 걸릴 수 있습니다.
                </p>
              </>
            ) : (
              <>
                {agentBuildMismatch(
                  host.agent?.artifact,
                  coordinatorBuildId,
                ) && (
                  <div className="warning-notice">
                    설치된 Farm 실행기가 coordinator와 다른 빌드입니다
                    {` (${host.agent?.artifact?.build_id || "버전 미상"} ≠ ${coordinatorBuildId})`}
                    . VM 정리는 새 실행기가 필요하므로, 먼저 실행기를
                    재설치하거나 이 옵션을 끄고 기록만 지우세요.
                  </div>
                )}
                <label className="checkbox-row">
                  <Checkbox.Root
                    checked={purging}
                    disabled={mismatch}
                    onCheckedChange={(checked) => setPurge(checked === true)}
                    className="checkbox"
                  >
                    <Checkbox.Indicator>
                      <Check size={12} strokeWidth={3} />
                    </Checkbox.Indicator>
                  </Checkbox.Root>
                  <span>
                    이 호스트의 Farm {farms}개와 VM·디스크도 삭제
                    <small>
                      {farms > 0
                        ? "farm을 순서대로 삭제한 뒤, 실행기가 보관 중인 남은 VM까지 정리합니다."
                        : "남아 있는 예약 VM이 있으면 함께 정리합니다."}
                    </small>
                  </span>
                </label>
                {snapshots > 0 && (
                  <label className="checkbox-row">
                    <Checkbox.Root
                      checked={purging && deleteSnapshots}
                      disabled={!purging}
                      onCheckedChange={(checked) =>
                        setDeleteSnapshots(checked === true)
                      }
                      className="checkbox"
                    >
                      <Checkbox.Indicator>
                        <Check size={12} strokeWidth={3} />
                      </Checkbox.Indicator>
                    </Checkbox.Root>
                    <span>
                      스냅샷 {snapshots}개도 삭제
                      <small>
                        {purging && deleteSnapshots
                          ? "내보내지 않은 스냅샷은 되돌릴 수 없습니다."
                          : `스냅샷은 ${place}에 남고, 지우려면 ${place}를 다시 등록해야 합니다.`}
                      </small>
                    </span>
                  </label>
                )}
                <p className="field-hint">
                  {purging
                    ? `farm이 만든 VM과 디스크만 지웁니다. ${local ? place : "이 서버"}에서 직접 만든 msb 샌드박스는 그대로 둡니다.`
                    : local
                      ? `호스트 기록만 지웁니다. ${place}의 VM·디스크는 남습니다.`
                      : "호스트 기록과 인증 정보만 지웁니다. 서버의 VM·디스크는 남습니다."}
                </p>
              </>
            )}
            {error ? <ErrorNotice error={error} /> : null}
          </div>
          <div className="modal-footer justify-end">
            <Dialog.Close render={<Button disabled={pending} />}>
              취소
            </Dialog.Close>
            <Button
              disabled={pending}
              variant={prepare ? "primary" : "danger"}
              onClick={async () => {
                setPending(true);
                setError(undefined);
                try {
                  await run(
                    !prepare && purging,
                    !prepare && purging && deleteSnapshots,
                  );
                  setOpen(false);
                } catch (e) {
                  setError(e);
                } finally {
                  setPending(false);
                }
              }}
            >
              {pending && <LoaderCircle size={14} className="animate-spin" />}
              {prepare ? "준비 작업 시작" : "등록 해제"}
            </Button>
          </div>
        </Modal>
      )}
    </Dialog.Root>
  );
}

export function RegisterHostForm({
  host,
  onPending,
  close,
  onRegistered,
}: {
  host?: RemoteHost;
  onPending: (value: boolean) => void;
  close: () => void;
  onRegistered: (host: RemoteHost) => Promise<void>;
}) {
  const [id, setId] = useState(host?.id ?? "");
  const [address, setAddress] = useState(host?.address ?? "");
  const [port, setPort] = useState(String(host?.port ?? 22));
  const [username, setUsername] = useState(host?.username ?? "");
  const [method, setMethod] = useState<"password" | "private_key">(
    "private_key",
  );
  const [secret, setSecret] = useState("");
  const [keyFile, setKeyFile] = useState("");
  const keyInput = useRef<HTMLInputElement>(null);
  const [passphrase, setPassphrase] = useState("");
  const [verified, setVerified] = useState(false);
  const [pending, setPending] = useState(false);
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState("");
  const [error, setError] = useState<unknown>();
  const fingerprint = useMutation({
    mutationFn: () => api.fingerprint(address.trim(), Number(port)),
  });
  const resetFingerprint = () => {
    fingerprint.reset();
    setVerified(false);
    setTestResult("");
  };
  const credentials = () =>
    method === "password"
      ? { method, password: secret }
      : { method, private_key: secret, passphrase: passphrase || null };
  const canTest =
    !!username &&
    !!secret &&
    verified &&
    !!fingerprint.data &&
    !fingerprint.isPending &&
    !pending &&
    !testing;
  const canRegister =
    /^[a-z0-9](?:[a-z0-9-]{0,46}[a-z0-9])?$/.test(id) &&
    !!username &&
    !!secret &&
    verified &&
    !!fingerprint.data &&
    !fingerprint.isPending &&
    !pending;
  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    if (!canRegister || !fingerprint.data) return;
    setError(undefined);
    setPending(true);
    onPending(true);
    // Credential requests intentionally bypass mutation/query caches. No browser persistence.
    try {
      const saved = await (
        host
          ? (body: Parameters<typeof api.registerHost>[0]) =>
              api.updateHostCredentials(host.id, body)
          : api.registerHost
      )({
        id,
        address: address.trim(),
        port: Number(port),
        username,
        fingerprint: fingerprint.data.fingerprint,
        credentials: credentials(),
      });
      setSecret("");
      setPassphrase("");
      await onRegistered(saved);
      close();
    } catch (e) {
      setError(e);
    } finally {
      setSecret("");
      setPassphrase("");
      setPending(false);
      onPending(false);
    }
  };
  return (
    <Modal
      title={host ? `${host.id} 인증 정보 변경` : "원격 호스트 등록"}
      description="먼저 서버 지문을 확인한 뒤 SSH 인증 정보를 입력하세요."
      className="create-modal remote-register-modal"
      closeDisabled={pending || testing || fingerprint.isPending}
    >
      <form onSubmit={(event) => void submit(event)} autoComplete="off">
        <div className="modal-body space-y-5">
          <div className="grid grid-cols-[1fr_95px] gap-3">
            <label className="field">
              <span>서버 IP 또는 호스트명</span>
              <Input
                className="input"
                value={address}
                required
                maxLength={253}
                disabled={pending || fingerprint.isPending || !!host}
                placeholder="10.0.0.21"
                onValueChange={(value) => {
                  setAddress(value);
                  resetFingerprint();
                }}
              />
            </label>
            <label className="field">
              <span>SSH 포트</span>
              <Input
                className="input"
                type="number"
                min={1}
                max={65535}
                required
                value={port}
                disabled={pending || fingerprint.isPending || !!host}
                onValueChange={(value) => {
                  setPort(value);
                  resetFingerprint();
                }}
              />
            </label>
          </div>
          <Button
            type="button"
            disabled={
              !address.trim() ||
              !Number.isInteger(Number(port)) ||
              Number(port) < 1 ||
              Number(port) > 65535 ||
              pending ||
              fingerprint.isPending
            }
            onClick={() => {
              setVerified(false);
              fingerprint.mutate();
            }}
          >
            <Fingerprint size={15} />
            {fingerprint.isPending ? "지문 조회 중…" : "서버 지문 조회"}
          </Button>
          {fingerprint.error && <ErrorNotice error={fingerprint.error} />}
          {fingerprint.data && (
            <div className="remote-key-confirm">
              <span className="field-label">
                확인되지 않은 서버 지문 · {fingerprint.data.algorithm}
              </span>
              <code>{fingerprint.data.fingerprint}</code>
              <p className="field-hint">
                서버 콘솔 등 신뢰할 수 있는 경로에서 확인한 지문과 비교하세요.
                이 조회 자체는 서버 신원을 보장하지 않습니다.
              </p>
              <label className="flex items-start gap-2 text-xs">
                <Checkbox.Root
                  className="checkbox"
                  checked={verified}
                  disabled={pending}
                  onCheckedChange={(value) => setVerified(value)}
                >
                  <Checkbox.Indicator>
                    <Check size={12} />
                  </Checkbox.Indicator>
                </Checkbox.Root>
                <span>신뢰할 수 있는 서버 지문과 일치함을 확인했습니다.</span>
              </label>
            </div>
          )}
          <div className="grid grid-cols-2 gap-3">
            <label className="field">
              <span>호스트 ID</span>
              <Input
                className="input"
                value={id}
                required
                maxLength={48}
                pattern="[a-z0-9]([a-z0-9-]{0,46}[a-z0-9])?"
                disabled={pending || !!host}
                placeholder="worker-01"
                onValueChange={setId}
              />
              <small>영문 소문자·숫자·하이픈, 최대 48자</small>
            </label>
            <label className="field">
              <span>SSH 사용자</span>
              <Input
                className="input"
                value={username}
                required
                maxLength={64}
                pattern="[a-zA-Z0-9_.-]+"
                disabled={pending || !!host}
                placeholder="ubuntu"
                onValueChange={setUsername}
              />
            </label>
          </div>
          <div>
            <span className="field-label" id="ssh-auth-label">
              인증 방식
            </span>
            <RadioGroup
              value={method}
              onValueChange={(value) => {
                setMethod(value as typeof method);
                setSecret("");
                setKeyFile("");
                setPassphrase("");
                setTestResult("");
              }}
              aria-labelledby="ssh-auth-label"
              disabled={pending}
              className="flex gap-5"
            >
              {[
                { value: "private_key", label: "SSH 키" },
                { value: "password", label: "비밀번호" },
              ].map((item) => (
                <label
                  key={item.value}
                  className="flex items-center gap-2 text-xs"
                >
                  <Radio.Root value={item.value} className="radio">
                    <Radio.Indicator className="radio-indicator" />
                  </Radio.Root>
                  {item.label}
                </label>
              ))}
            </RadioGroup>
          </div>
          {method === "private_key" ? (
            <>
              <label className="field">
                <span className="flex items-center justify-between gap-2">
                  SSH 개인 키
                  <button
                    type="button"
                    className="text-link"
                    disabled={pending || testing}
                    onClick={() => keyInput.current?.click()}
                  >
                    <Upload size={13} />
                    파일 선택
                  </button>
                </span>
                <input
                  ref={keyInput}
                  type="file"
                  className="sr-only"
                  aria-label="SSH 개인 키 파일"
                  disabled={pending || testing}
                  onChange={(event) => {
                    const file = event.target.files?.[0];
                    event.target.value = "";
                    if (!file) return;
                    if (file.size > 65536) {
                      setError(
                        new Error("개인 키 파일은 64KB를 넘을 수 없습니다."),
                      );
                      return;
                    }
                    void file.text().then(
                      (text) => {
                        setSecret(text.replace(/\r\n/g, "\n"));
                        setKeyFile(file.name);
                        setTestResult("");
                        setError(undefined);
                      },
                      () =>
                        setError(new Error("개인 키 파일을 읽지 못했습니다.")),
                    );
                  }}
                />
                <textarea
                  className="input remote-key-input"
                  required
                  value={secret}
                  maxLength={65536}
                  disabled={pending || testing}
                  spellCheck={false}
                  placeholder="-----BEGIN OPENSSH PRIVATE KEY-----"
                  onChange={(e) => {
                    setSecret(e.target.value);
                    setKeyFile("");
                    setTestResult("");
                  }}
                />
                <small>
                  {keyFile
                    ? `${keyFile} 파일을 불러왔습니다. 직접 고쳐도 됩니다.`
                    : "키 파일을 선택하거나 내용을 붙여넣으세요."}
                </small>
              </label>
              <label className="field">
                <span>
                  키 암호 <span className="optional">선택</span>
                </span>
                <Input
                  className="input"
                  type="password"
                  value={passphrase}
                  maxLength={4096}
                  disabled={pending}
                  autoComplete="new-password"
                  onValueChange={setPassphrase}
                />
              </label>
            </>
          ) : (
            <label className="field">
              <span>SSH 비밀번호</span>
              <Input
                className="input"
                type="password"
                value={secret}
                required
                maxLength={4096}
                disabled={pending}
                autoComplete="new-password"
                onValueChange={setSecret}
              />
            </label>
          )}
          <p className="field-hint flex items-start gap-2">
            <KeyRound size={15} className="shrink-0 mt-0.5" />
            인증 정보는 Coordinator에서 암호화해 보관합니다. 브라우저 저장소에
            저장하거나 조회 응답으로 반환하지 않습니다.
          </p>
          {testResult && (
            <p className="field-hint" role="status">
              접속되었습니다. {testResult}
            </p>
          )}
          {error ? <ErrorNotice error={error} /> : null}
        </div>
        <div className="modal-footer">
          <span className="field-hint">
            {host
              ? "변경 후 SSH 점검을 시작합니다."
              : "등록 후 SSH 점검을 시작합니다."}
          </span>
          <div className="flex gap-2">
            <Button
              type="button"
              disabled={!canTest}
              onClick={() => {
                if (!fingerprint.data) return;
                setTesting(true);
                setError(undefined);
                setTestResult("");
                onPending(true);
                void api
                  .testConnection({
                    id: id || "probe",
                    address: address.trim(),
                    port: Number(port),
                    username,
                    fingerprint: fingerprint.data.fingerprint,
                    credentials: credentials(),
                  })
                  .then((result) => setTestResult(result.identity))
                  .catch(setError)
                  .finally(() => {
                    setTesting(false);
                    onPending(false);
                  });
              }}
            >
              {testing ? (
                <LoaderCircle size={14} className="animate-spin" />
              ) : (
                <RefreshCw size={14} />
              )}
              {testing ? "접속 확인 중" : "접속 테스트"}
            </Button>
            <Button
              type="submit"
              variant="primary"
              disabled={!canRegister || testing}
            >
              {pending ? (
                <LoaderCircle size={14} className="animate-spin" />
              ) : (
                <Plus size={14} />
              )}
              {host ? "인증 정보 저장" : "호스트 등록"}
            </Button>
          </div>
        </div>
      </form>
    </Modal>
  );
}
