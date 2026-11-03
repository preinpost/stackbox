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
import { useState } from "react";
import {
  api,
  catalogQuery,
  farmsQuery,
  hostQuery,
  hostsQuery,
  keys,
  type Farm,
  type Job,
  type Phase,
  type RemoteHost,
} from "../api/client";
import {
  componentInfo,
  hostLabel,
  hostNotice,
  hostReady,
  remoteAgentNeedsUpdate,
  componentLabel,
  farmComponents,
  farmNodesRunning,
  isDeployableFarm,
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

async function refresh(client: ReturnType<typeof useQueryClient>) {
  await Promise.all([
    client.invalidateQueries({ queryKey: keys.farms }),
    client.invalidateQueries({ queryKey: keys.jobs }),
    client.invalidateQueries({ queryKey: keys.host }),
    client.invalidateQueries({ queryKey: keys.hosts }),
  ]);
}
function publicHostName(hostId: string, hosts: RemoteHost[] = []) {
  const chosen = hosts.find((h) => h.id === hostId);
  if (chosen?.kind === "local") return "localhost";
  return chosen && /^[a-z0-9.-]+$/i.test(chosen.address) ? chosen.address : "";
}
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
      {open && (
        <CreateFarmForm
          close={() => setOpen(false)}
          onPending={setPending}
          initialHost={hostId}
        />
      )}
    </Dialog.Root>
  );
}
export function CreateFarmForm({
  close,
  onPending,
  initialHost,
}: {
  close: () => void;
  onPending: (pending: boolean) => void;
  initialHost?: string;
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
  const farms = useQuery(farmsQuery());
  const components = resolveComponents(
    catalog.data?.components ?? [],
    requested,
  );
  const plan = useQuery({
    queryKey: ["plan", components, computes],
    queryFn: ({ signal }) => api.plan(components, computes, signal),
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
  const overBudget =
    remaining !== null && plan.data && plan.data.memory_mib > remaining;
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
            external_host: hostName,
            host_id: hostId,
            ...(owner.trim() ? { owner: owner.trim() } : {}),
          });
        }}
      >
        <div className="modal-body space-y-6">
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
  const allowed =
    ["created", "failed", "ready"].includes(farm.status) &&
    validNodes &&
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
