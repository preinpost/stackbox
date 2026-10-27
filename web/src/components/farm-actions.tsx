import { Dialog } from "@base-ui/react/dialog";
import { Input } from "@base-ui/react/input";
import { Checkbox } from "@base-ui/react/checkbox";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
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
  type Host,
  type Job,
  type Phase,
  type RemoteHost,
} from "../api/client";
import {
  agentBuildMismatch,
  componentInfo,
  componentLabel,
  farmComponents,
  isAllInOneFarm,
  memory,
  phases,
  resolveComponents,
} from "../lib";
import {
  Button,
  CopyText,
  ErrorNotice,
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
export function localHostReady(host: Host) {
  return (
    host.msb_available &&
    host.golden_image_available &&
    host.hypervisor_available
  );
}
export const remoteAgentNeedsUpdate = (host: RemoteHost) =>
  Boolean(host.agent && !host.agent.artifact?.supports_single_vm);
// Creation requires an updated agent plus a fresh inspection and an idle host.
export function remoteHostReady(host: RemoteHost, coordinatorBuildId?: string) {
  return (
    host.status === "ready" &&
    host.agent?.artifact?.supports_single_vm === true &&
    !agentBuildMismatch(host.agent?.artifact, coordinatorBuildId) &&
    !host.active_job &&
    Date.now() / 1000 - (host.inspection?.checked_at ?? 0) <= 300
  );
}
function publicHostName(target: string, hosts: RemoteHost[] = []) {
  if (target === "local") return "localhost";
  const chosen = hosts.find((h) => `remote:${h.id}` === target);
  return chosen && /^[a-z0-9.-]+$/i.test(chosen.address) ? chosen.address : "";
}
export function CreateFarmDialog({
  prominent = false,
  target,
  label = "Farm 만들기",
  disabled,
}: {
  prominent?: boolean;
  target?: string;
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
          initialTarget={target}
        />
      )}
    </Dialog.Root>
  );
}
export function CreateFarmForm({
  close,
  onPending,
  initialTarget = "local",
}: {
  close: () => void;
  onPending: (pending: boolean) => void;
  initialTarget?: string;
}) {
  const client = useQueryClient();
  const navigate = useNavigate();
  const catalog = useQuery(catalogQuery());
  const host = useQuery(hostQuery());
  const remoteHosts = useQuery(hostsQuery());
  const [name, setName] = useState("");
  const [owner, setOwner] = useState("");
  const [selected, setSelected] = useState<string[]>([]);
  const [target, setTarget] = useState(initialTarget);
  const [hostName, setHostName] = useState(() =>
    publicHostName(initialTarget, remoteHosts.data),
  );
  const hostId = target.startsWith("remote:") ? target.slice(7) : undefined;
  const remote = remoteHosts.data?.find((h) => h.id === hostId);
  const farms = useQuery(farmsQuery());
  const components = resolveComponents(
    catalog.data?.components ?? [],
    selected,
  );
  const plan = useQuery({
    queryKey: ["plan", components],
    queryFn: ({ signal }) => api.plan(components, signal),
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
  const capacity = hostId ? remote?.capacity : host.data;
  const remaining = capacity
    ? capacity.memory_budget_mib - capacity.memory_reserved_mib
    : null;
  const overBudget =
    remaining !== null && plan.data && plan.data.memory_mib > remaining;
  const targetError = hostId ? remoteHosts.error : host.error;
  const hostUnavailable = hostId
    ? !remote || !remoteHostReady(remote, host.data?.build_id)
    : host.data && !localHostReady(host.data);
  return (
    <Modal
      title="새 farm 만들기"
      description="Farm마다 VM 한 대를 만들고 선택한 OpenStack 서비스를 설치합니다."
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
            external_host: hostName,
            ...(hostId ? { host_id: hostId } : {}),
            ...(owner.trim() ? { owner: owner.trim() } : {}),
          });
        }}
      >
        <div className="modal-body space-y-6">
          <div>
            <span className="field-label">실행 호스트</span>
            <SelectInput
              value={target}
              onChange={(value) => {
                setTarget(value);
                setHostName(publicHostName(value, remoteHosts.data));
              }}
              disabled={mutation.isPending}
              label="실행 호스트"
              items={[
                { value: "local", label: "로컬 · Coordinator" },
                ...(remoteHosts.data ?? []).map((h) => {
                  const ready = remoteHostReady(h, host.data?.build_id);
                  return {
                    value: `remote:${h.id}`,
                    label: `${h.id} · ${ready ? "원격 실행" : remoteAgentNeedsUpdate(h) ? "실행기 업데이트 필요" : "준비 필요"}`,
                    // Keep the current choice selectable so a preselected host that went stale still renders.
                    disabled: !ready && `remote:${h.id}` !== target,
                  };
                }),
              ]}
            />
            <p className="field-hint mt-2">
              {hostId
                ? "VM 한 대가 선택한 서버에서 실행됩니다. 서비스 포트는 해당 서버의 모든 IPv4 인터페이스에 바인딩됩니다."
                : "Coordinator의 로컬 microsandbox에서 VM 한 대를 실행합니다."}
              {remaining !== null &&
                ` · 추가 예약 가능 ${memory(Math.max(0, remaining))}`}
            </p>
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
            <span className="field-label">OpenStack 서비스</span>
            <div className="component-options">
              {catalog.data?.components.map((c) => {
                const included = components.includes(c.id);
                // A dependency of another selection stays on until that selection is removed.
                const neededBy = catalog.data.components
                  .filter(
                    (other) =>
                      other.id !== c.id &&
                      selected.includes(other.id) &&
                      resolveComponents(catalog.data.components, [
                        other.id,
                      ]).includes(c.id),
                  )
                  .map((other) => componentLabel(other.id));
                const locked =
                  c.required || !c.available || neededBy.length > 0;
                return (
                  <label
                    className={`component-option ${included ? "selected" : ""} ${!c.available ? "unavailable" : ""}`}
                    key={c.id}
                  >
                    <Checkbox.Root
                      checked={included}
                      disabled={locked || mutation.isPending}
                      onCheckedChange={(checked) =>
                        setSelected((current) =>
                          checked
                            ? [...current, c.id]
                            : current.filter((id) => id !== c.id),
                        )
                      }
                      className="checkbox"
                    >
                      <Checkbox.Indicator>
                        <Check size={12} strokeWidth={3} />
                      </Checkbox.Indicator>
                    </Checkbox.Root>
                    <span className="component-text">
                      <strong>{componentLabel(c.id)}</strong>
                      <small>{componentInfo[c.id]?.description}</small>
                    </span>
                    <span className="optional">
                      {c.required
                        ? "필수"
                        : !c.available
                          ? "준비 중"
                          : neededBy.length
                            ? `${neededBy.join("·")}에 필요`
                            : "선택"}
                    </span>
                  </label>
                );
              })}
            </div>
            <p className="field-hint mt-2">
              의존성은 자동으로 포함됩니다. 선택한 서비스와 DB·메시지 큐는 같은
              VM의 컨테이너로 실행되며, 추가 서비스의 메모리도 이 VM에
              합산됩니다.
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
                선택한 모든 서비스를 이 VM에 설치합니다. 생성 후 상세 화면에서
                배포를 시작하세요.
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
                void host.refetch();
                void remoteHosts.refetch();
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
          {hostUnavailable && (
            <div className="warning-notice">
              {hostId
                ? remote &&
                  agentBuildMismatch(
                    remote.agent?.artifact,
                    host.data?.build_id,
                  )
                  ? "설치된 Farm 실행기가 이 coordinator와 다른 소스에서 빌드되었습니다. `task agent-build`로 다시 빌드한 뒤 Hosts에서 실행기를 재설치하세요."
                  : remote && remoteAgentNeedsUpdate(remote)
                    ? "설치된 Farm 실행기는 단일 VM 생성을 지원하지 않습니다. Hosts에서 최신 Farm 실행기를 설치하세요. 기존 Farm의 조회와 삭제는 계속 사용할 수 있습니다."
                    : "Hosts에서 환경 준비와 단일 VM용 Farm 실행기 설치를 완료하세요. 마지막 SSH 점검이 5분을 지났다면 점검을 갱신하세요."
                : "Hosts의 로컬 호스트에서 msb·가상화·golden image 상태를 확인하세요."}
            </div>
          )}
          {mutation.error && <ErrorNotice error={mutation.error} />}
        </div>
        <div className="modal-footer">
          <span className="flex items-center gap-1.5 text-xs text-stone-500">
            <ShieldCheck size={14} />
            Farm마다 독립된 VM 한 대
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
  const validNodes =
    isAllInOneFarm(farm) &&
    farm.nodes.length === 1 &&
    farm.nodes.every(
      (n) => String(n.role) === "all-in-one" && n.status === "running",
    );
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
          !isAllInOneFarm(farm)
            ? "기존 다중 VM Farm은 재배포할 수 없습니다. 단일 VM Farm을 새로 만드세요."
            : undefined
        }
      >
        <Play size={14} />
        {farm.active_job
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
  const client = useQueryClient();
  const mutation = useMutation({
    mutationFn: () => api.delete(farm.id),
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
        description="이 farm의 모든 VM과 연결된 디스크가 삭제됩니다."
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
              Farm 삭제
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
    isAllInOneFarm(farm) &&
    ["created", "broken", "ready"].includes(farm.status) &&
    farm.nodes.length === 1 &&
    farm.nodes[0].status === "running";
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
