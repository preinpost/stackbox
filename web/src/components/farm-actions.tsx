import { Dialog } from "@base-ui/react/dialog";
import { Input } from "@base-ui/react/input";
import { Checkbox } from "@base-ui/react/checkbox";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import {
  ArrowRight,
  Boxes,
  Check,
  Cpu,
  HardDrive,
  LoaderCircle,
  Play,
  Plus,
  RefreshCw,
  ShieldCheck,
  Trash2,
} from "lucide-react";
import { useState, type ReactNode } from "react";
import {
  api,
  catalogQuery,
  farmsQuery,
  hostQuery,
  hostsQuery,
  keys,
  snapshotsQuery,
  type ComputeSize,
  type Farm,
  type Job,
  type Phase,
  type RemoteHost,
  type Snapshot,
} from "../api/client";
import {
  componentInfo,
  hostLabel,
  hostNotice,
  hostReady,
  remoteAgentNeedsUpdate,
  componentLabel,
  farmComponents,
  awaitsCompute,
  farmNodesRunning,
  isDeployableFarm,
  isMultiNodeFarm,
  memory,
  phases,
  resolveComponents,
} from "../lib";
import {
  Button,
  CopyText,
  ErrorNotice,
  ForgetOption,
  Loading,
  Modal,
  SelectInput,
} from "./ui";
import { ReleaseFarmForm } from "./release-create";

async function refresh(client: ReturnType<typeof useQueryClient>) {
  await Promise.all([
    client.invalidateQueries({ queryKey: keys.farms }),
    client.invalidateQueries({ queryKey: keys.jobs }),
    client.invalidateQueries({ queryKey: keys.host }),
    client.invalidateQueries({ queryKey: keys.hosts }),
  ]);
}
/** Compute templates a new compute of `farm` can boot from; the API also checks the
 * OpenStack release. */
export const computeTemplatesFor = (
  farm: Farm,
  snapshots: Snapshot[] = [],
  arch?: string | null,
) =>
  snapshots.filter(
    (s) =>
      s.template === "compute" &&
      ["ready", "exporting"].includes(s.status) &&
      (!arch || s.arch === arch) &&
      s.plan.components.join(",") === (farm.plan?.components ?? []).join(","),
  );
function publicHostName(hostId: string, hosts: RemoteHost[] = []) {
  const chosen = hosts.find((h) => h.id === hostId);
  if (chosen?.kind === "local") return "localhost";
  return chosen && /^[a-z0-9.-]+$/i.test(chosen.address) ? chosen.address : "";
}
/** Compute VM sizes the create form offers. Few disk sizes keep few golden images per host. */
export const computeSizes: { name: string; size: ComputeSize }[] = [
  { name: "기본", size: { cpus: 2, memory_mib: 3072, disk_mib: 32768 } },
  { name: "중간", size: { cpus: 4, memory_mib: 6144, disk_mib: 65536 } },
  { name: "큰", size: { cpus: 4, memory_mib: 10240, disk_mib: 65536 } },
  { name: "최대", size: { cpus: 8, memory_mib: 16384, disk_mib: 131072 } },
];

export function CreateFarmDialog({
  prominent = false,
  hostId,
  label = "Farm 만들기",
  disabled,
}: {
  prominent?: boolean;
  hostId?: string;
  label?: string;
  disabled?: boolean;
}) {
  const [open, setOpen] = useState(false);
  const [pending, setPending] = useState(false);
  const [mode, setMode] = useState<"release" | "custom">("release");
  const header = (
    <div className="field">
      <span>만드는 방법</span>
      <div
        className="segmented-control"
        role="radiogroup"
        aria-label="만드는 방법"
      >
        {(
          [
            { value: "release", label: "릴리스로 만들기" },
            { value: "custom", label: "구성 직접 고르기" },
          ] as const
        ).map((item) => (
          <button
            key={item.value}
            type="button"
            role="radio"
            aria-checked={mode === item.value}
            className={`flex-1 ${mode === item.value ? "selected" : ""}`}
            disabled={pending}
            onClick={() => setMode(item.value)}
          >
            {item.label}
          </button>
        ))}
      </div>
    </div>
  );
  return (
    <Dialog.Root
      open={open}
      onOpenChange={(next) => {
        if (!pending) setOpen(next);
      }}
    >
      <Dialog.Trigger
        disabled={disabled}
        render={
          <Button variant="primary" className={prominent ? "mt-5" : ""} />
        }
      >
        <Plus size={16} />
        {label}
      </Dialog.Trigger>
      {open &&
        (mode === "release" ? (
          <ReleaseFarmForm
            close={() => setOpen(false)}
            onPending={setPending}
            initialHost={hostId}
            header={header}
          />
        ) : (
          <CreateFarmForm
            close={() => setOpen(false)}
            onPending={setPending}
            initialHost={hostId}
            header={header}
          />
        ))}
    </Dialog.Root>
  );
}
export function CreateFarmForm({
  close,
  onPending,
  initialHost,
  header,
}: {
  close: () => void;
  onPending: (pending: boolean) => void;
  initialHost?: string;
  header?: ReactNode;
}) {
  const client = useQueryClient();
  const navigate = useNavigate();
  const catalog = useQuery(catalogQuery());
  const coordinator = useQuery(hostQuery());
  const hosts = useQuery(hostsQuery());
  const buildId = coordinator.data?.build_id;
  const [name, setName] = useState("");
  const [owner, setOwner] = useState("");
  const [selected, setSelected] = useState<string[]>([]);
  // Compute VMs beside a control VM; 0 is one all-in-one VM. Compute VMs bring Nova.
  const [computes, setComputes] = useState(0);
  const [sizeIndex, setSizeIndex] = useState(0);
  // Compute VMs placed on another host than the control VM's (`compute-2` → host id).
  const [placed, setPlaced] = useState<Record<string, string>>({});
  const compute = computes > 0 ? computeSizes[sizeIndex].size : undefined;
  const requested =
    computes > 0 && !selected.includes("nova")
      ? [...selected, "nova"]
      : selected;
  // Until the user picks one, default to the given host, else the first ready one.
  const [picked, setPicked] = useState(initialHost);
  const hostId =
    picked ??
    (hosts.data?.find((h) => hostReady(h, buildId)) ?? hosts.data?.[0])?.id;
  const [editedHostName, setHostName] = useState<string>();
  const hostName = editedHostName ?? publicHostName(hostId ?? "", hosts.data);
  const target = hosts.data?.find((h) => h.id === hostId);
  // Only the computes that exist, on hosts other than the control VM's.
  const placement = Object.fromEntries(
    Object.entries(placed).filter(
      ([node, host]) =>
        host !== hostId && Number(node.slice("compute-".length)) <= computes,
    ),
  );
  const readyHosts = (hosts.data ?? []).filter((h) => hostReady(h, buildId));
  const canSpan =
    computes > 0 &&
    !!coordinator.data?.relay_url &&
    (hosts.data ?? []).filter((h) => hostReady(h, buildId)).length > 1;
  const farms = useQuery(farmsQuery());
  const components = resolveComponents(
    catalog.data?.components ?? [],
    requested,
  );
  const plan = useQuery({
    queryKey: ["plan", components, computes, compute],
    queryFn: ({ signal }) => api.plan(components, computes, signal, compute),
    enabled: catalog.isSuccess,
    staleTime: Infinity,
  });
  const mutation = useMutation({
    mutationFn: api.create,
    onMutate: () => onPending(true),
    onSettled: () => onPending(false),
    onSuccess: async (job) => {
      await refresh(client);
      close();
      await navigate({ to: "/farms/$farmId", params: { farmId: job.farm_id } });
    },
  });
  const duplicate = farms.data?.some((f) => f.id === name);
  const capacity = target?.capacity;
  const remaining = capacity
    ? capacity.memory_budget_mib - capacity.memory_reserved_mib
    : null;
  // Computes placed elsewhere count against their own host.
  const placedMemory = plan.data
    ? Object.keys(placement).length * (plan.data.nodes[1]?.memory_mib ?? 0)
    : 0;
  const overBudget =
    remaining !== null &&
    plan.data &&
    plan.data.memory_mib - placedMemory > remaining;
  const targetError = hosts.error || coordinator.error;
  const noHosts = hosts.isSuccess && !hosts.data.length;
  const hostUnavailable = !target || !hostReady(target, buildId);
  return (
    <Modal
      title="새 farm 만들기"
      description="선택한 구성으로 VM을 만들고 OpenStack 서비스를 설치합니다."
      className="create-modal"
      closeDisabled={mutation.isPending}
    >
      <form
        onSubmit={(event) => {
          event.preventDefault();
          if (!plan.data || duplicate || overBudget || hostUnavailable) return;
          mutation.mutate({
            id: name,
            components,
            computes,
            ...(compute ? { compute } : {}),
            ...(Object.keys(placement).length ? { placement } : {}),
            external_host: hostName,
            host_id: hostId,
            ...(owner.trim() ? { owner: owner.trim() } : {}),
          });
        }}
      >
        <div className="modal-body space-y-6">
          {header}
          <div>
            <span className="field-label">실행 호스트</span>
            {noHosts ? (
              <div className="warning-notice">
                등록된 호스트가 없습니다.{" "}
                <Link to="/hosts" className="text-link" onClick={close}>
                  Hosts
                </Link>
                에서 로컬 호스트나 원격 서버를 등록하고 환경 준비를 마치세요.
              </div>
            ) : (
              <>
                <SelectInput
                  value={hostId ?? ""}
                  onChange={(value) => {
                    setPicked(value);
                    setHostName(undefined);
                  }}
                  disabled={mutation.isPending || !hosts.data}
                  label="실행 호스트"
                  items={(hosts.data ?? []).map((h) => {
                    const ready = hostReady(h, buildId);
                    return {
                      value: h.id,
                      label: `${hostLabel(h)}${ready ? "" : remoteAgentNeedsUpdate(h) ? " · 실행기 업데이트 필요" : " · 준비 필요"}`,
                      // Keep the current choice selectable so a preselected host that went stale still renders.
                      disabled: !ready && h.id !== hostId,
                    };
                  })}
                />
                <p className="field-hint mt-2">
                  {target?.kind === "local"
                    ? "coordinator(farmd)가 실행 중인 머신의 microsandbox에서 farm의 VM을 실행합니다."
                    : "farm의 VM이 선택한 서버에서 실행됩니다. 서비스 포트는 해당 서버의 모든 IPv4 인터페이스에 바인딩됩니다."}
                  {remaining !== null &&
                    ` · 추가 예약 가능 ${memory(Math.max(0, remaining))}`}
                </p>
              </>
            )}
          </div>
          <div className="grid gap-4 sm:grid-cols-2">
            <label className="field">
              <span>
                Farm ID <span className="text-emerald-600">*</span>
              </span>
              <Input
                autoFocus
                value={name}
                onValueChange={setName}
                className="input font-mono"
                placeholder="dev-openstack"
                required
                minLength={1}
                maxLength={48}
                pattern={"[a-z0-9]([a-z0-9\\-]{0,46}[a-z0-9])?"}
                disabled={mutation.isPending}
              />
              <small>소문자, 숫자, 하이픈 · 최대 48자</small>
              {duplicate && (
                <small className="text-red-600">
                  이미 사용 중인 Farm ID입니다.
                </small>
              )}
            </label>
            <label className="field">
              <span>
                담당자 <span className="optional">선택</span>
              </span>
              <Input
                value={owner}
                onValueChange={setOwner}
                className="input"
                placeholder="이름 또는 팀"
                maxLength={256}
                disabled={mutation.isPending}
              />
              <small>환경을 관리하는 담당자를 표시합니다.</small>
            </label>
          </div>
          <div>
            <span className="field-label">VM 구성</span>
            <SelectInput
              value={String(computes)}
              onChange={(value) => setComputes(Number(value))}
              disabled={mutation.isPending}
              label="VM 구성"
              items={[
                { value: "0", label: "VM 1대 · all-in-one" },
                ...[1, 2, 3].map((n) => ({
                  value: String(n),
                  label: `Control 1 + Compute ${n} · VM ${n + 1}대`,
                })),
              ]}
            />
            <p className="field-hint mt-2">
              {computes > 0
                ? "control VM이 API·DB·MQ·네트워크 노드와 Kolla를 맡고, compute VM에서 Nova 인스턴스가 실행됩니다. VM끼리는 호스트의 링크 스위치로 연결되며, 상세 화면에서 compute VM을 고립시켜 evacuate를 시험할 수 있습니다."
                : "모든 서비스를 VM 한 대에 설치합니다. Nova를 고르면 compute VM이 1대 붙습니다."}
            </p>
          </div>
          {computes > 0 && (
            <div>
              <span className="field-label">Compute VM 사양</span>
              <SelectInput
                value={String(sizeIndex)}
                onChange={(value) => setSizeIndex(Number(value))}
                disabled={mutation.isPending}
                label="Compute VM 사양"
                items={computeSizes.map(({ name, size }, index) => ({
                  value: String(index),
                  label: `${name} · ${size.cpus} vCPU · 메모리 ${memory(size.memory_mib)} · 디스크 ${memory(size.disk_mib)}`,
                }))}
              />
              <p className="field-hint mt-2">
                compute VM마다 같은 사양입니다. nova-compute·libvirt·OVS가 약
                0.8 GiB를 쓰고 나머지가 인스턴스 몫입니다. 인스턴스 디스크는
                디스크 크기 안에 만들어지며, 처음 쓰는 디스크 크기는 호스트에서
                golden image를 한 번 더 만듭니다(약 35초).
              </p>
            </div>
          )}
          {canSpan && (
            <div>
              <span className="field-label">VM 배치</span>
              <div className="grid gap-3 sm:grid-cols-2">
                {/* The control VM's host is the farm's host, the same choice as 실행 호스트. */}
                <SelectInput
                  value={hostId ?? ""}
                  onChange={(value) => {
                    setPicked(value);
                    setHostName(undefined);
                  }}
                  disabled={mutation.isPending}
                  label="control 호스트"
                  items={readyHosts.map((h) => ({
                    value: h.id,
                    label: `control · ${hostLabel(h)}`,
                  }))}
                />
                {Array.from({ length: computes }, (_, i) => {
                  const node = `compute-${i + 1}`;
                  return (
                    <SelectInput
                      key={node}
                      value={placement[node] ?? hostId ?? ""}
                      onChange={(value) =>
                        setPlaced((current) => ({ ...current, [node]: value }))
                      }
                      disabled={mutation.isPending}
                      label={`${node} 호스트`}
                      items={readyHosts.map((h) => ({
                        value: h.id,
                        label: `${node} · ${hostLabel(h)}${h.id === hostId ? " (control과 같은 호스트)" : ""}`,
                      }))}
                    />
                  );
                })}
              </div>
              <p className="field-hint mt-2">
                control VM의 호스트는 위의 실행 호스트와 같습니다. 다른 호스트에
                둔 compute VM은 coordinator의 relay(
                {coordinator.data?.relay_url})를 거쳐 control VM과 같은 망에
                연결되며, 링크는 종단 간 암호화됩니다.
              </p>
            </div>
          )}
          <div>
            <span className="field-label">OpenStack 서비스</span>
            <div className="component-options">
              {catalog.data?.components.map((c) => {
                const included = components.includes(c.id);
                // A dependency of another selection stays on until that selection is removed.
                const neededBy = catalog.data.components
                  .filter(
                    (other) =>
                      other.id !== c.id &&
                      requested.includes(other.id) &&
                      resolveComponents(catalog.data.components, [
                        other.id,
                      ]).includes(c.id),
                  )
                  .map((other) => componentLabel(other.id));
                const forCompute = computes > 0 && c.id === "nova";
                const locked =
                  c.required ||
                  !c.available ||
                  forCompute ||
                  neededBy.length > 0;
                return (
                  <label
                    className={`component-option ${included ? "selected" : ""} ${!c.available ? "unavailable" : ""}`}
                    key={c.id}
                  >
                    <Checkbox.Root
                      checked={included}
                      disabled={locked || mutation.isPending}
                      onCheckedChange={(checked) => {
                        setSelected((current) =>
                          checked
                            ? [...current, c.id]
                            : current.filter((id) => id !== c.id),
                        );
                        // Nova instances run on a compute VM; the plan adds one anyway.
                        if (checked && c.id === "nova" && computes === 0) {
                          setComputes(1);
                        }
                      }}
                      className="checkbox"
                    >
                      <Checkbox.Indicator>
                        <Check size={12} strokeWidth={3} />
                      </Checkbox.Indicator>
                    </Checkbox.Root>
                    <span className="component-text">
                      <span className="component-head">
                        <strong>{componentLabel(c.id)}</strong>
                        <span className="component-badge">
                          {c.required
                            ? "필수"
                            : !c.available
                              ? "준비 중"
                              : forCompute
                                ? "Compute VM에 필요"
                                : neededBy.length
                                  ? `${neededBy.join("·")}에 필요`
                                  : "선택"}
                        </span>
                      </span>
                      <small>{componentInfo[c.id]?.description}</small>
                    </span>
                  </label>
                );
              })}
            </div>
            <p className="field-hint mt-2">
              {computes > 0
                ? "의존성은 자동으로 포함됩니다. Nova를 뺀 서비스와 DB·메시지 큐는 control VM의 컨테이너로 실행됩니다."
                : "의존성은 자동으로 포함됩니다. 선택한 서비스와 DB·메시지 큐는 같은 VM의 컨테이너로 실행되며, 추가 서비스의 메모리도 이 VM에 합산됩니다."}
            </p>
          </div>
          <label className="field">
            <span>외부 접속 호스트</span>
            <Input
              value={hostName}
              onValueChange={setHostName}
              className="input font-mono"
              required
              maxLength={253}
              placeholder="localhost"
              disabled={mutation.isPending}
            />
            <small>
              OpenStack public URL에 사용할 VM 호스트의 DNS 이름 또는 IPv4 주소
            </small>
          </label>
          {plan.data && (
            <div className="plan-summary">
              <span className="eyebrow">ENVIRONMENT PLAN</span>
              <div>
                <span>
                  <Boxes size={17} />
                  <strong>{plan.data.nodes.length}</strong> VM
                </span>
                <span>
                  <Cpu size={17} />
                  <strong>{memory(plan.data.memory_mib)}</strong> 메모리
                </span>
                <span>
                  <HardDrive size={17} />
                  <strong>{memory(plan.data.disk_mib)}</strong> 디스크
                </span>
              </div>
              <p>
                {plan.data.computes
                  ? `Control ${memory(plan.data.nodes[0].memory_mib)} + Compute ${memory(plan.data.nodes[1].memory_mib)} × ${plan.data.computes}. `
                  : "선택한 모든 서비스를 이 VM에 설치합니다. "}
                생성 후 상세 화면에서 배포를 시작하세요.
              </p>
            </div>
          )}
          {(catalog.isPending || plan.isPending) && (
            <Loading label="리소스 계획 계산 중" />
          )}
          {(catalog.error || plan.error || targetError) && (
            <ErrorNotice
              error={catalog.error || plan.error || targetError}
              retry={() => {
                void catalog.refetch();
                void plan.refetch();
                void coordinator.refetch();
                void hosts.refetch();
              }}
            />
          )}
          {overBudget && (
            <div className="warning-notice">
              예약 가능한 메모리가 부족합니다. 현재 여유:{" "}
              {memory(Math.max(0, remaining ?? 0))}. 다른 farm을 삭제하거나
              호스트의 예약 한도를 조절하세요.
            </div>
          )}
          {target && hostUnavailable && (
            <div className="warning-notice">{hostNotice(target, buildId)}</div>
          )}
          {mutation.error && <ErrorNotice error={mutation.error} />}
        </div>
        <div className="modal-footer">
          <span className="flex items-center gap-1.5 text-xs text-stone-500">
            <ShieldCheck size={14} />
            {computes > 0
              ? `Farm 전용 VM ${computes + 1}대와 내부망`
              : "Farm마다 독립된 VM 한 대"}
          </span>
          <div className="flex gap-2">
            <Dialog.Close render={<Button disabled={mutation.isPending} />}>
              취소
            </Dialog.Close>
            <Button
              type="submit"
              variant="primary"
              disabled={
                mutation.isPending ||
                !plan.data ||
                !capacity ||
                Boolean(targetError) ||
                duplicate ||
                Boolean(overBudget) ||
                Boolean(hostUnavailable)
              }
            >
              {mutation.isPending ? (
                <LoaderCircle size={15} className="animate-spin" />
              ) : (
                <Plus size={15} />
              )}
              {mutation.isPending ? "생성 요청 중" : "Farm 만들기"}
            </Button>
          </div>
        </div>
      </form>
    </Modal>
  );
}
export function DeployFarmDialog({
  farm,
  lastJob,
}: {
  farm: Farm;
  lastJob?: Job;
}) {
  const [open, setOpen] = useState(false);
  const [from, setFrom] = useState<Phase>("prepare");
  const client = useQueryClient();
  const mutation = useMutation({
    mutationFn: () => api.deploy(farm.id, from),
    onSuccess: async () => {
      await refresh(client);
      setOpen(false);
    },
  });
  const validNodes = isDeployableFarm(farm) && farmNodesRunning(farm);
  // Kolla came with the control template; such a farm only gains computes.
  const allowed =
    ["created", "failed", "ready"].includes(farm.status) &&
    validNodes &&
    !awaitsCompute(farm) &&
    !farm.active_job;
  const resumable =
    (lastJob?.status === "failed" || lastJob?.status === "interrupted") &&
    lastJob.kind === "deploy" &&
    Boolean(lastJob.phase);
  return (
    <Dialog.Root
      open={open}
      onOpenChange={(v) => {
        if (!mutation.isPending) {
          setOpen(v);
          if (v) {
            mutation.reset();
            setFrom(resumable ? lastJob!.phase! : "prepare");
          }
        }
      }}
    >
      <Dialog.Trigger
        render={<Button variant="primary" disabled={!allowed} />}
        title={
          !isDeployableFarm(farm)
            ? "기존 다중 VM Farm은 재배포할 수 없습니다. Farm을 새로 만드세요."
            : awaitsCompute(farm)
              ? "control 템플릿으로 만든 farm에는 아직 compute VM이 없습니다. Compute를 추가하세요."
              : undefined
        }
      >
        <Play size={14} />
        {farm.status === "creating"
          ? "VM 생성 중..."
          : farm.active_job
            ? "작업 진행 중"
            : resumable
              ? "배포 재개"
              : farm.status === "ready"
                ? "다시 배포"
                : "OpenStack 배포"}
      </Dialog.Trigger>
      <Modal
        title={resumable ? "OpenStack 배포 재개" : "OpenStack 배포"}
        description="Kolla 단계별 진행 상황과 로그를 실시간으로 확인할 수 있습니다."
        closeDisabled={mutation.isPending}
      >
        <form
          onSubmit={(event) => {
            event.preventDefault();
            mutation.mutate();
          }}
        >
          <div className="modal-body space-y-5">
            <div className="action-context">
              <Boxes size={20} />
              <div>
                <strong className="font-mono">{farm.id}</strong>
                <p>
                  VM 한 대에{" "}
                  {farmComponents(farm).map(componentLabel).join(" · ")} 배포
                </p>
              </div>
            </div>
            <label className="field">
              <span>시작 단계</span>
              <SelectInput
                label="배포 시작 단계"
                value={from}
                onChange={setFrom}
                items={phases.map((p) => ({
                  value: p.id,
                  label: `${p.label} — ${p.description}`,
                }))}
                disabled={mutation.isPending}
              />
            </label>
            <p className="field-hint">
              처음 배포할 때는 Prepare부터 시작하세요. 실패한 배포는 저장된
              단계부터 재개할 수 있습니다. 호스트당 배포는 한 번에 하나씩
              실행됩니다.
            </p>
            {mutation.error && <ErrorNotice error={mutation.error} />}
          </div>
          <div className="modal-footer justify-end">
            <Dialog.Close render={<Button disabled={mutation.isPending} />}>
              취소
            </Dialog.Close>
            <Button
              type="submit"
              variant="primary"
              disabled={mutation.isPending}
            >
              {mutation.isPending ? (
                <LoaderCircle size={15} className="animate-spin" />
              ) : (
                <ArrowRight size={15} />
              )}
              배포 시작
            </Button>
          </div>
        </form>
      </Modal>
    </Dialog.Root>
  );
}
export function AddComputeDialog({
  farm,
  lastJob,
}: {
  farm: Farm;
  lastJob?: Job;
}) {
  const resumable =
    lastJob?.kind === "expand" &&
    ["failed", "interrupted"].includes(lastJob.status) &&
    Boolean(lastJob.target_node);
  const [open, setOpen] = useState(false);
  const [hostId, setHostId] = useState(
    lastJob?.target_host ?? farm.host_id ?? "",
  );
  const resumedTemplate = resumable ? (lastJob?.snapshot_id ?? "") : "";
  const [mode, setMode] = useState<"clean" | "snapshot">(
    resumedTemplate ? "snapshot" : "clean",
  );
  const [template, setTemplate] = useState(resumedTemplate);
  const client = useQueryClient();
  const hosts = useQuery(hostsQuery());
  const coordinator = useQuery(hostQuery());
  const snapshots = useQuery(snapshotsQuery());
  const templates = computeTemplatesFor(
    farm,
    snapshots.data,
    coordinator.data?.arch,
  );
  const localHost = hosts.data?.find((host) => host.kind === "local");
  // Templates are kept on the local host, beside a control VM that runs there.
  const templateHost = farm.host_id === localHost?.id;
  const plan = farm.plan;
  const computes = plan?.computes ?? 0;
  const next = computes + 1;
  const ready = (hosts.data ?? []).filter((host) =>
    hostReady(host, coordinator.data?.build_id),
  );
  const remote = hostId !== farm.host_id;
  const relay = coordinator.data?.relay_url;
  const allowed =
    isDeployableFarm(farm) &&
    (farm.status === "ready" || (farm.status === "failed" && resumable)) &&
    !farm.active_job &&
    isMultiNodeFarm(farm) &&
    computes < 3;
  const fromSnapshot = mode === "snapshot";
  const canSubmit =
    allowed &&
    Boolean(hostId) &&
    (!remote || Boolean(relay)) &&
    (!fromSnapshot || (Boolean(template) && !remote && templateHost));
  const mutation = useMutation({
    mutationFn: () =>
      api.addCompute(
        farm.id,
        hostId || undefined,
        fromSnapshot ? template : undefined,
      ),
    onSuccess: async () => {
      await refresh(client);
      setOpen(false);
    },
  });
  return (
    <Dialog.Root
      open={open}
      onOpenChange={(value) => {
        if (!mutation.isPending) {
          setOpen(value);
          if (value) {
            mutation.reset();
            setHostId(lastJob?.target_host ?? farm.host_id ?? "");
            setMode(resumedTemplate ? "snapshot" : "clean");
            setTemplate(resumedTemplate);
          }
        }
      }}
    >
      <Dialog.Trigger
        render={<Button disabled={!allowed} />}
        title={
          computes >= 3
            ? "한 farm에는 compute VM을 3대까지 둘 수 있습니다."
            : !isMultiNodeFarm(farm)
              ? "all-in-one farm에는 compute를 추가할 수 없습니다. control과 compute로 이루어진 farm을 새로 만드세요."
              : "배포가 끝난 farm에 다음 compute VM을 추가합니다."
        }
      >
        <Cpu size={15} />
        Compute 추가
      </Dialog.Trigger>
      <Modal
        title={`Compute ${next} 추가`}
        description="control VM은 그대로 두고, 새 compute VM만 준비해 Kolla에 등록합니다."
        closeDisabled={mutation.isPending}
      >
        <form
          onSubmit={(event) => {
            event.preventDefault();
            if (canSubmit) mutation.mutate();
          }}
        >
          <div className="modal-body space-y-5">
            <div className="action-context">
              <Cpu size={20} />
              <div>
                <strong className="font-mono">
                  {farm.id}-{lastJob?.target_node ?? `compute-${next}`}
                </strong>
                <p>
                  기존 compute와 같은 크기 · {plan?.compute?.cpus ?? 2} vCPU ·{" "}
                  {memory(plan?.compute?.memory_mib ?? 3072)}
                </p>
              </div>
            </div>
            <div className="field">
              <span>설치 방식</span>
              <div className="segmented-control" role="radiogroup">
                {(
                  [
                    { value: "clean", label: "클린 설치" },
                    { value: "snapshot", label: "스냅샷에서" },
                  ] as const
                ).map((item) => (
                  <button
                    key={item.value}
                    type="button"
                    role="radio"
                    aria-checked={mode === item.value}
                    className={`flex-1 ${mode === item.value ? "selected" : ""}`}
                    disabled={mutation.isPending || resumable}
                    onClick={() => {
                      setMode(item.value);
                      if (item.value === "snapshot") {
                        setHostId(farm.host_id ?? "");
                        if (!template && templates[0])
                          setTemplate(templates[0].id);
                      }
                    }}
                  >
                    {item.label}
                  </button>
                ))}
              </div>
            </div>
            {fromSnapshot && (
              <label className="field">
                <span>Compute 템플릿</span>
                <SelectInput
                  label="compute 템플릿 스냅샷"
                  value={template}
                  onChange={setTemplate}
                  items={(resumedTemplate && !templates.length
                    ? [{ id: resumedTemplate }]
                    : templates
                  ).map((snapshot) => ({
                    value: snapshot.id,
                    label: snapshot.id,
                  }))}
                  disabled={mutation.isPending || resumable}
                />
                <small>
                  이 farm과 같은 서비스로 얼린 compute 템플릿만 보입니다.
                  OpenStack 릴리스가 다르면 추가가 거부됩니다.
                </small>
              </label>
            )}
            {fromSnapshot && !templates.length && !resumedTemplate && (
              <div className="warning-notice">
                쓸 수 있는 compute 템플릿이 없습니다. 배포된 farm의 얼리기에서
                compute VM 하나를 템플릿으로 얼려두세요.
              </div>
            )}
            {fromSnapshot && !templateHost && (
              <div className="warning-notice">
                compute 템플릿은 로컬 호스트에 있으므로, control VM이 로컬
                호스트에서 도는 farm에만 쓸 수 있습니다.
              </div>
            )}
            <label className="field">
              <span>호스트</span>
              <SelectInput
                label={`compute-${next} 호스트`}
                value={hostId}
                onChange={setHostId}
                items={ready.map((host) => ({
                  value: host.id,
                  label: hostLabel(host),
                }))}
                disabled={mutation.isPending || resumable || fromSnapshot}
              />
            </label>
            {remote && !relay && (
              <div className="warning-notice">
                다른 호스트의 compute를 붙이려면 먼저 Hosts에서 호스트 간
                relay를 설정하세요.
              </div>
            )}
            {resumable && (
              <div className="info-notice">
                <RefreshCw size={18} />
                <div>
                  <strong>실패한 확장을 이어서 진행합니다.</strong> 새 VM은 이미
                  만들어졌을 수 있으며, 이 작업은 준비와 Kolla 등록부터 다시
                  실행합니다.
                </div>
              </div>
            )}
            <p className="field-hint">
              새 VM은 farm의 링크에 붙고, control VM의 Kolla inventory에
              추가됩니다. 기존 인스턴스는 새 compute로 자동 이동하지 않습니다.
              {fromSnapshot
                ? " 템플릿 디스크로 부팅해 노드 identity를 지우고, 이미지 pull 없이 Kolla reconfigure만 실행합니다."
                : " 다른 호스트를 고르면 중계 링크를 거칩니다."}
            </p>
            {mutation.error && <ErrorNotice error={mutation.error} />}
          </div>
          <div className="modal-footer justify-end">
            <Dialog.Close render={<Button disabled={mutation.isPending} />}>
              취소
            </Dialog.Close>
            <Button
              type="submit"
              variant="primary"
              disabled={!canSubmit || mutation.isPending}
            >
              {mutation.isPending ? (
                <LoaderCircle size={15} className="animate-spin" />
              ) : (
                <Plus size={15} />
              )}
              추가
            </Button>
          </div>
        </form>
      </Modal>
    </Dialog.Root>
  );
}
export function DeleteFarmDialog({ farm }: { farm: Farm }) {
  const [open, setOpen] = useState(false);
  const [confirm, setConfirm] = useState("");
  const [forget, setForget] = useState(false);
  const client = useQueryClient();
  const mutation = useMutation({
    mutationFn: () => api.delete(farm.id, forget),
    onSuccess: async () => {
      await refresh(client);
      setOpen(false);
    },
  });
  return (
    <Dialog.Root
      open={open}
      onOpenChange={(v) => {
        if (!mutation.isPending) {
          setOpen(v);
          if (v) {
            setConfirm("");
            setForget(false);
            mutation.reset();
          }
        }
      }}
    >
      <Dialog.Trigger
        render={
          <Button
            variant="ghost"
            disabled={Boolean(farm.active_job) || farm.status === "deleted"}
            className="delete-trigger"
          />
        }
        aria-label="Farm 삭제"
      >
        <Trash2 size={16} />
      </Dialog.Trigger>
      <Modal
        title="Farm을 삭제할까요?"
        description={
          forget
            ? "이 farm의 coordinator 기록만 삭제합니다."
            : "이 farm의 모든 VM과 연결된 디스크가 삭제됩니다."
        }
        closeDisabled={mutation.isPending}
      >
        <form
          onSubmit={(event) => {
            event.preventDefault();
            if (confirm === farm.id) mutation.mutate();
          }}
        >
          <div className="modal-body space-y-5">
            <div className="warning-notice">
              이 작업은 되돌릴 수 없습니다. 필요한 이미지나 데이터를 먼저
              보관하세요.
            </div>
            <ForgetOption
              checked={forget}
              onChange={setForget}
              disabled={mutation.isPending}
              left="VM과 디스크"
            />
            <div className="field">
              <span>
                확인을 위해 <CopyText value={farm.id} /> 입력
              </span>
              <Input
                aria-label={`확인을 위해 ${farm.id} 입력`}
                className="input font-mono"
                value={confirm}
                onValueChange={setConfirm}
                placeholder={farm.id}
                disabled={mutation.isPending}
                autoComplete="off"
              />
            </div>
            {mutation.error && <ErrorNotice error={mutation.error} />}
          </div>
          <div className="modal-footer justify-end">
            <Dialog.Close render={<Button disabled={mutation.isPending} />}>
              취소
            </Dialog.Close>
            <Button
              type="submit"
              variant="danger"
              disabled={confirm !== farm.id || mutation.isPending}
            >
              {mutation.isPending ? (
                <LoaderCircle size={15} className="animate-spin" />
              ) : (
                <Trash2 size={15} />
              )}
              {forget ? "기록만 삭제" : "Farm 삭제"}
            </Button>
          </div>
        </form>
      </Modal>
    </Dialog.Root>
  );
}

/** Re-checks a deployed farm's services after its VM started from disk; nothing is deployed. */
export function CheckServicesButton({
  farm,
  deployed,
}: {
  farm: Farm;
  deployed: boolean;
}) {
  const client = useQueryClient();
  const mutation = useMutation({
    mutationFn: () => api.settle(farm.id),
    onSuccess: () => refresh(client),
  });
  const allowed =
    deployed &&
    !farm.active_job &&
    isDeployableFarm(farm) &&
    ["created", "broken", "ready"].includes(farm.status) &&
    farmNodesRunning(farm);
  return (
    <Button
      onClick={() => mutation.mutate()}
      disabled={!allowed || mutation.isPending}
      title={
        deployed
          ? "VM을 다시 시작한 뒤 서비스가 응답하는지 확인합니다. 배포는 다시 실행하지 않습니다."
          : "배포를 한 번 완료한 farm에서 사용할 수 있습니다."
      }
    >
      {mutation.isPending ? (
        <LoaderCircle size={14} className="animate-spin" />
      ) : (
        <RefreshCw size={14} />
      )}
      서비스 확인
    </Button>
  );
}
