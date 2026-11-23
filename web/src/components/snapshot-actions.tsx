import { Dialog } from "@base-ui/react/dialog";
import { Input } from "@base-ui/react/input";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import {
  Cloud,
  CloudDownload,
  CloudUpload,
  CopyPlus,
  Download,
  HardDrive,
  LoaderCircle,
  RefreshCw,
  Snowflake,
  Trash2,
  Upload,
} from "lucide-react";
import { useState } from "react";
import {
  api,
  bucketObjectsQuery,
  bucketsQuery,
  busySnapshot,
  farmsQuery,
  hostQuery,
  hostsQuery,
  keys,
  snapshotsQuery,
  type Farm,
  type Snapshot,
} from "../api/client";
import {
  archiveSummary,
  bytes,
  componentLabel,
  dateTime,
  hostLabel,
  hostNotice,
  hostReady,
  awaitsCompute,
  farmNodesRunning,
  isDeployableFarm,
  isMultiNodeFarm,
  memory,
  remoteAgentNeedsUpdate,
  snapshotHost,
  snapshotOrigin,
} from "../lib";
import { bucketLocation } from "./bucket-actions";
import {
  Button,
  CopyText,
  ErrorNotice,
  ForgetOption,
  Loading,
  Modal,
  SelectInput,
} from "./ui";

const idPattern = "[a-z0-9]([a-z0-9\\-]{0,46}[a-z0-9])?";

async function refresh(client: ReturnType<typeof useQueryClient>) {
  await Promise.all([
    client.invalidateQueries({ queryKey: keys.snapshots }),
    client.invalidateQueries({ queryKey: keys.farms }),
    client.invalidateQueries({ queryKey: keys.jobs }),
    client.invalidateQueries({ queryKey: keys.host }),
  ]);
}
/** Dialog open state that ignores close requests while its mutation runs. */
function useDialog(pending: boolean, onOpen?: () => void) {
  const [open, setOpen] = useState(false);
  return {
    open,
    setOpen,
    onOpenChange: (next: boolean) => {
      if (pending) return;
      setOpen(next);
      if (next) onOpen?.();
    },
  };
}

/** VMs of a deployed multi-node farm that can be frozen as templates: the control VM,
 * while every VM runs on the farm's own host, which keeps its snapshots, and the computes
 * there. */
export const templateNodes = (farm: Farm) =>
  farm.status === "ready" && isMultiNodeFarm(farm)
    ? [
        ...(farm.shares?.length ? [] : ["control"]),
        ...(farm.plan?.nodes ?? [])
          .filter((spec) => spec.role === "compute" && spec.index)
          .map((spec) => `compute-${spec.index}`)
          .filter(
            (suffix) =>
              !farm.shares?.some((share) => share.nodes.includes(suffix)),
          ),
      ]
    : [];
const templateLabel = (node: string) =>
  node === "control" ? "control (control 템플릿)" : `${node} (compute 템플릿)`;

/** The farm's host keeps the snapshot, so the farm must be on a registered host. */
export const canFreeze = (farm: Farm, hostIds: string[] = []) =>
  Boolean(farm.host_id && hostIds.includes(farm.host_id)) &&
  !farm.active_job &&
  Boolean(farm.plan && farm.exposure) &&
  isDeployableFarm(farm) &&
  ["created", "ready", "failed"].includes(farm.status) &&
  farmNodesRunning(farm);

export function FreezeFarmDialog({ farm }: { farm: Farm }) {
  const client = useQueryClient();
  const hosts = useQuery(hostsQuery()).data;
  const coordinator = useQuery(hostQuery());
  // The farm's host keeps the snapshot, and its architecture decides where it boots.
  const farmHost = hosts?.find((h) => h.id === farm.host_id);
  const local = farmHost?.kind === "local";
  const snapshots = useQuery(snapshotsQuery());
  // A farm without computes yet is frozen as a control template only.
  const whole = !awaitsCompute(farm);
  const initialNode = whole ? "" : "control";
  const [node, setNode] = useState(initialNode);
  const suggest = (node: string) =>
    node
      ? `${farm.id.slice(0, 36)}-${node}-tpl`
      : `${farm.id.slice(0, 42)}-${farm.status === "ready" ? "clean" : "base"}`;
  const suggested = suggest(initialNode);
  const [id, setId] = useState(suggested);
  const [note, setNote] = useState("");
  const templates = templateNodes(farm);
  const mutation = useMutation({
    mutationFn: () =>
      api.freeze(farm.id, {
        id,
        ...(note.trim() ? { note: note.trim() } : {}),
        ...(node ? { node } : {}),
      }),
    onSuccess: async () => {
      await refresh(client);
      dialog.setOpen(false);
    },
  });
  const dialog = useDialog(mutation.isPending, () => {
    setNode(initialNode);
    setId(suggested);
    setNote("");
    mutation.reset();
  });
  const duplicate = snapshots.data?.some((s) => s.id === id);
  return (
    <Dialog.Root open={dialog.open} onOpenChange={dialog.onOpenChange}>
      <Dialog.Trigger
        render={
          <Button
            disabled={
              !canFreeze(
                farm,
                hosts?.map((h) => h.id),
              )
            }
          />
        }
        title={
          !farmHost
            ? "farm의 호스트가 등록되어 있지 않아 얼릴 수 없습니다."
            : local
              ? "VM을 스냅샷으로 얼려둡니다."
              : `VM을 스냅샷으로 얼려 ${farmHost.id}에 둡니다.`
        }
      >
        <Snowflake size={16} />
        얼리기
      </Dialog.Trigger>
      <Modal
        title="Farm 얼리기"
        description="VM의 디스크를 스냅샷으로 저장합니다. 이 스냅샷으로 같은 환경의 farm을 빠르게 만들 수 있습니다."
        closeDisabled={mutation.isPending}
      >
        <form
          onSubmit={(event) => {
            event.preventDefault();
            if (!duplicate) mutation.mutate();
          }}
        >
          <div className="modal-body space-y-5">
            <div className="action-context">
              <Snowflake size={20} />
              <div>
                <strong className="font-mono">
                  {node ? `${farm.id}-${node}` : farm.id}
                </strong>
                <p>
                  {node === "control"
                    ? "control VM 하나를 control 템플릿으로 저장합니다. 이 템플릿으로 compute 없는 farm을 만든 뒤 compute를 추가해 클러스터를 조립합니다."
                    : node
                      ? "이 compute VM 하나를 compute 템플릿으로 저장합니다. 같은 서비스와 릴리스의 farm에 compute를 빠르게 추가할 때 씁니다."
                      : farm.status === "ready"
                        ? "배포된 OpenStack을 그대로 저장합니다. 복원한 farm은 바로 Ready가 됩니다."
                        : "OpenStack 배포 전 상태입니다. 복원한 farm은 VM 준비됨 상태로 시작합니다."}
                </p>
              </div>
            </div>
            {templates.length > 0 && (
              <label className="field">
                <span>대상</span>
                <SelectInput
                  label="얼릴 대상"
                  value={node}
                  onChange={(value) => {
                    setNode(value);
                    setId(suggest(value));
                  }}
                  items={[
                    ...(whole ? [{ value: "", label: "farm 전체" }] : []),
                    ...templates.map((suffix) => ({
                      value: suffix,
                      label: templateLabel(suffix),
                    })),
                  ]}
                  disabled={mutation.isPending}
                />
              </label>
            )}
            <label className="field">
              <span>스냅샷 ID</span>
              <Input
                autoFocus
                value={id}
                onValueChange={setId}
                className="input font-mono"
                required
                maxLength={48}
                pattern={idPattern}
                disabled={mutation.isPending}
              />
              <small>소문자, 숫자, 하이픈 · 최대 48자</small>
              {duplicate && (
                <small className="field-error">
                  이미 사용 중인 스냅샷 ID입니다.
                </small>
              )}
            </label>
            <label className="field">
              <span>
                메모 <span className="optional">선택</span>
              </span>
              <Input
                value={note}
                onValueChange={setNote}
                className="input"
                placeholder="예: Nova·Neutron 포함 clean 설치"
                maxLength={512}
                disabled={mutation.isPending}
              />
            </label>
            {node === "control" ? (
              <div className="warning-notice">
                farm 전체처럼 compute를 먼저 멈추고 control을 먼저 켜지만,
                control VM만 저장합니다. 원본 farm의 OpenStack은 바꾸지
                않습니다. 인스턴스가 있는 farm은 얼릴 수 없습니다. 이 템플릿으로
                만든 farm은 원본의 compute 등록(nova-compute, placement, Neutron
                agent)을 지우고 compute 없이 시작합니다.
              </div>
            ) : node ? (
              <div className="warning-notice">
                {node}만 잠시 정지했다가 다시 시작합니다. 인스턴스가 있는
                compute는 템플릿으로 얼릴 수 없습니다. 노드 identity(Nova
                compute_id, machine-id, OVS system-id)는 템플릿으로 compute를
                추가할 때 지워집니다.
              </div>
            ) : (
              <div className="warning-notice">
                데이터베이스를 깨끗하게 저장하기 위해 VM을 잠시 정지했다가 다시
                시작합니다. 여러 VM farm은 compute를 먼저 멈추고 control을 먼저
                켭니다. 서비스가 다시 응답할 때까지 몇 분 정도 걸립니다. 이
                스냅샷은{" "}
                {snapshotHost(
                  local || !farmHost?.inspection
                    ? {
                        arch: coordinator.data?.arch ?? undefined,
                        platform: coordinator.data?.os,
                      }
                    : {
                        arch: farmHost.inspection.arch,
                        platform: farmHost.inspection.os,
                      },
                )}
                에서만 복원할 수 있습니다.
                {!local &&
                  farmHost &&
                  ` 원본은 ${farmHost.id}에 두고, 다른 호스트에서 쓸 때 사본을 복사합니다.`}
              </div>
            )}
            {mutation.error && <ErrorNotice error={mutation.error} />}
          </div>
          <div className="modal-footer justify-end">
            <Dialog.Close render={<Button disabled={mutation.isPending} />}>
              취소
            </Dialog.Close>
            <Button
              type="submit"
              variant="primary"
              disabled={mutation.isPending || duplicate}
            >
              {mutation.isPending ? (
                <LoaderCircle size={16} className="animate-spin" />
              ) : (
                <Snowflake size={16} />
              )}
              얼리기 시작
            </Button>
          </div>
        </form>
      </Modal>
    </Dialog.Root>
  );
}

export function RestoreSnapshotDialog({
  snapshot,
  foreignArch = false,
}: {
  snapshot: Snapshot;
  foreignArch?: boolean;
}) {
  const client = useQueryClient();
  const navigate = useNavigate();
  const farms = useQuery(farmsQuery());
  const coordinator = useQuery(hostQuery());
  const buildId = coordinator.data?.build_id;
  const hosts = useQuery(hostsQuery()).data;
  const originId = snapshotOrigin(snapshot, hosts);
  const hostArchOf = (h: NonNullable<typeof hosts>[number]) =>
    h.inspection?.arch ?? (h.kind === "local" ? coordinator.data?.arch : undefined);
  const archCompatible = (h: NonNullable<typeof hosts>[number]) => {
    const arch = hostArchOf(h);
    return !arch || arch === snapshot.arch;
  };
  const hasCopy = (h: NonNullable<typeof hosts>[number]) =>
    Boolean(snapshot.copies?.some((c) => c.host_id === h.id));
  const originRegistered = Boolean(
    originId && hosts?.some((h) => h.id === originId),
  );
  // Default to the host keeping the original when registered and ready, else the first
  // ready host of the same architecture (preferring one that already has a copy).
  const defaultHost =
    hosts?.find((h) => h.id === originId && hostReady(h, buildId) && archCompatible(h)) ??
    hosts?.find((h) => hasCopy(h) && hostReady(h, buildId) && archCompatible(h)) ??
    hosts?.find((h) => hostReady(h, buildId) && archCompatible(h)) ??
    hosts?.find((h) => h.id === originId) ??
    hosts?.find((h) => archCompatible(h)) ??
    hosts?.[0];
  const [pickedHost, setPickedHost] = useState<string>();
  const target = hosts?.find((h) => h.id === pickedHost) ?? defaultHost;
  const [id, setId] = useState("");
  const [owner, setOwner] = useState("");
  const [editedHostName, setEditedHostName] = useState<string>();
  const defaultExternalHost = (h?: NonNullable<typeof hosts>[number]) => {
    if (!h || h.kind === "local") return "localhost";
    return /^[a-z0-9.-]+$/i.test(h.address) ? h.address : "localhost";
  };
  const hostName = editedHostName ?? defaultExternalHost(target);
  const mutation = useMutation({
    mutationFn: () =>
      api.create({
        id,
        host_id: target?.id,
        snapshot_id: snapshot.id,
        external_host: hostName,
        ...(owner.trim() ? { owner: owner.trim() } : {}),
      }),
    onSuccess: async (job) => {
      await refresh(client);
      dialog.setOpen(false);
      await navigate({ to: "/farms/$farmId", params: { farmId: job.farm_id } });
    },
  });
  const dialog = useDialog(mutation.isPending, () => {
    setPickedHost(undefined);
    setId("");
    setOwner("");
    setEditedHostName(undefined);
    mutation.reset();
  });
  const duplicate = farms.data?.some((f) => f.id === id);
  const capacity = target?.capacity;
  const remaining = capacity
    ? capacity.memory_budget_mib - capacity.memory_reserved_mib
    : null;
  const targetArch = target ? hostArchOf(target) : undefined;
  const targetWrongArch = Boolean(targetArch && targetArch !== snapshot.arch);
  const targetIsOrigin = Boolean(target && target.id === originId);
  const targetHasCopy = Boolean(target && hasCopy(target));
  const needsCopy = Boolean(target && !targetIsOrigin && !targetHasCopy);
  const copyBlockedByOrigin = Boolean(
    target && !targetIsOrigin && !targetHasCopy && !originRegistered && !snapshot.archive?.bucket,
  );
  const needsMtty = snapshot.plan.gpu?.mode === "mtty";
  const mttyMissing = Boolean(
    needsMtty && target && target.inspection?.mtty_available !== true,
  );
  const hostMissing = !target || !hostReady(target, buildId);
  const overBudget = remaining !== null && snapshot.plan.memory_mib > remaining;
  const anyCompatibleHost = hosts
    ? hosts.some((h) => archCompatible(h))
    : !foreignArch;
  const ready =
    ["ready", "exporting"].includes(snapshot.status) && anyCompatibleHost;
  const control = snapshot.template === "control";
  const canSubmit =
    !duplicate &&
    !overBudget &&
    !hostMissing &&
    !targetWrongArch &&
    !copyBlockedByOrigin &&
    !mttyMissing;
  return (
    <Dialog.Root open={dialog.open} onOpenChange={dialog.onOpenChange}>
      <Dialog.Trigger render={<Button variant="primary" disabled={!ready} />}>
        <CopyPlus size={16} />
        Farm 만들기
      </Dialog.Trigger>
      <Modal
        title={
          control ? "Control 템플릿에서 farm 만들기" : "스냅샷에서 farm 만들기"
        }
        description={
          control
            ? "얼려둔 control VM만 복사해 compute 없는 farm으로 시작합니다. 원본의 compute 등록은 지워지고, Compute 추가로 compute-1부터 clean 설치하거나 compute 템플릿에서 붙입니다."
            : "얼려둔 VM을 복사해 새 farm으로 시작합니다. OpenStack을 다시 설치하지 않습니다."
        }
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
              <Snowflake size={20} />
              <div>
                <strong className="font-mono">{snapshot.id}</strong>
                <p>
                  {snapshot.plan.components.map(componentLabel).join(" · ")} ·{" "}
                  {memory(snapshot.plan.memory_mib)}
                  {control
                    ? " · control VM만, compute 0대"
                    : snapshot.deployed
                      ? " · 배포 완료"
                      : " · 배포 전"}
                </p>
                <p>
                  {snapshotHost(snapshot)}에서 얼린 게스트 ·{" "}
                  {target
                    ? `${target.id}에 복원합니다`
                    : "복원할 호스트를 선택하세요"}
                </p>
              </div>
            </div>
            <div className="field">
              <span>실행 호스트</span>
              {hosts?.length ? (
                <>
                  <SelectInput
                    label="실행 호스트"
                    value={target?.id ?? ""}
                    onChange={(value) => {
                      setPickedHost(value);
                      setEditedHostName(undefined);
                    }}
                    disabled={mutation.isPending}
                    items={hosts.map((h) => {
                      const readyHost = hostReady(h, buildId);
                      const compatible = archCompatible(h);
                      const arch = hostArchOf(h);
                      const note = !compatible
                        ? `${arch} · 아키텍처 다름`
                        : !readyHost
                          ? remoteAgentNeedsUpdate(h)
                            ? "실행기 업데이트 필요"
                            : "준비 필요"
                          : h.id === originId
                            ? "원본 보관"
                            : hasCopy(h)
                              ? "사본 보관"
                              : "복사 후 복원";
                      return {
                        value: h.id,
                        label: `${hostLabel(h)} · ${note}`,
                        disabled: (!readyHost || !compatible) && h.id !== target?.id,
                      };
                    })}
                  />
                  <small>
                    {remaining !== null &&
                      `여유 메모리 ${memory(Math.max(0, remaining))}`}
                    {needsCopy
                      ? `${remaining !== null ? " · " : ""}${
                          snapshot.archive?.bucket
                            ? `버킷(${snapshot.archive.bucket})에서 내려받은 뒤 복원합니다`
                            : "원본 호스트에서 복사한 뒤 복원합니다"
                        }`
                      : ""}
                  </small>
                </>
              ) : (
                <div className="warning-notice">
                  등록된 호스트가 없습니다. Hosts에서 호스트를 먼저 등록하고
                  환경 준비를 마치세요.
                </div>
              )}
            </div>
            <div className="grid gap-4 sm:grid-cols-2">
              <label className="field">
                <span>
                  Farm ID <span className="field-required">*</span>
                </span>
                <Input
                  autoFocus
                  value={id}
                  onValueChange={setId}
                  className="input font-mono"
                  placeholder="dev-openstack"
                  required
                  maxLength={48}
                  pattern={idPattern}
                  disabled={mutation.isPending}
                />
                {duplicate && (
                  <small className="field-error">
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
              </label>
            </div>
            <label className="field">
              <span>외부 접속 호스트</span>
              <Input
                value={hostName}
                onValueChange={setEditedHostName}
                className="input font-mono"
                required
                maxLength={253}
                disabled={mutation.isPending}
              />
              <small>
                새 farm의 public 포트로 OpenStack 서비스 카탈로그를 다시
                등록합니다.
              </small>
            </label>
            {targetWrongArch && (
              <div className="warning-notice">
                이 스냅샷은 {snapshot.arch}에서 얼렸습니다. 선택한 호스트(
                {target?.id}, {targetArch})에서는 복원할 수 없습니다.
              </div>
            )}
            {mttyMissing && (
              <div className="warning-notice">
                이 스냅샷은 mtty GPU를 사용합니다. 선택한 호스트({target?.id})는
                mtty 커널 검증을 마치지 않았습니다.
              </div>
            )}
            {copyBlockedByOrigin && (
              <div className="warning-notice">
                원본을 보관하던 호스트({snapshot.host_id ?? "로컬 호스트"})가
                등록되어 있지 않아 복사할 수 없습니다. 원본 호스트를 다시
                등록하거나 이미 사본이 있는 호스트를 선택하세요.
              </div>
            )}
            {overBudget && (
              <div className="warning-notice">
                예약 가능한 메모리가 부족합니다. 현재 여유:{" "}
                {memory(Math.max(0, remaining ?? 0))}.
              </div>
            )}
            {hostMissing && (
              <div className="warning-notice">
                {target
                  ? hostNotice(target, buildId)
                  : snapshot.host_id
                    ? `스냅샷 원본은 ${snapshot.host_id}에 있습니다. Hosts에서 이 호스트를 다시 등록하세요.`
                    : "Hosts에서 호스트를 등록하고 환경 준비를 마치세요."}
              </div>
            )}
            {mutation.error && <ErrorNotice error={mutation.error} />}
          </div>
          <div className="modal-footer justify-end">
            <Dialog.Close render={<Button disabled={mutation.isPending} />}>
              취소
            </Dialog.Close>
            <Button
              type="submit"
              variant="primary"
              disabled={mutation.isPending || !canSubmit}
            >
              {mutation.isPending ? (
                <LoaderCircle size={16} className="animate-spin" />
              ) : (
                <CopyPlus size={16} />
              )}
              Farm 만들기
            </Button>
          </div>
        </form>
      </Modal>
    </Dialog.Root>
  );
}

/** Where an archive goes to, or comes from. */
function TransferMode({
  value,
  onChange,
  fileLabel,
  hasBuckets,
  disabled,
}: {
  value: "file" | "bucket";
  onChange: (value: "file" | "bucket") => void;
  fileLabel: string;
  hasBuckets: boolean;
  disabled?: boolean;
}) {
  return (
    <div className="segmented-control" role="radiogroup">
      {(
        [
          { value: "bucket", label: "S3 버킷", icon: <Cloud size={16} /> },
          { value: "file", label: fileLabel, icon: <HardDrive size={16} /> },
        ] as const
      ).map((item) => (
        <button
          key={item.value}
          type="button"
          role="radio"
          aria-checked={value === item.value}
          className={`flex flex-1 items-center justify-center gap-1.5 ${value === item.value ? "selected" : ""}`}
          disabled={disabled || (item.value === "bucket" && !hasBuckets)}
          title={
            item.value === "bucket" && !hasBuckets
              ? "Snapshots 화면 아래에서 S3 버킷을 먼저 등록하세요."
              : undefined
          }
          onClick={() => onChange(item.value)}
        >
          {item.icon}
          {item.label}
        </button>
      ))}
    </div>
  );
}

export function ExportSnapshotDialog({ snapshot }: { snapshot: Snapshot }) {
  const client = useQueryClient();
  const host = useQuery(hostQuery());
  const buckets = useQuery(bucketsQuery()).data ?? [];
  const [mode, setMode] = useState<"file" | "bucket">("file");
  const [path, setPath] = useState("");
  const [bucketId, setBucketId] = useState("");
  const [name, setName] = useState("");
  const bucket = buckets.find((b) => b.id === bucketId);
  // Only the name is typed; the prefix and the extension imports look for are fixed.
  const key = `${bucket?.prefix ?? ""}${name.trim().replace(/\.msb$/i, "") || snapshot.id}.msb`;
  const mutation = useMutation({
    mutationFn: () =>
      api.exportSnapshot(
        snapshot.id,
        mode === "bucket"
          ? { bucket: bucketId, key }
          : path.trim()
            ? { path: path.trim() }
            : {},
      ),
    onSuccess: async () => {
      await refresh(client);
      dialog.setOpen(false);
    },
  });
  const dialog = useDialog(mutation.isPending, () => {
    setMode(buckets.length ? "bucket" : "file");
    setPath("");
    setBucketId(buckets[0]?.id ?? "");
    setName("");
    mutation.reset();
  });
  const defaultPath = host.data
    ? `${host.data.state_dir}/exports/${snapshot.id}.msb`
    : `exports/${snapshot.id}.msb`;
  return (
    <Dialog.Root open={dialog.open} onOpenChange={dialog.onOpenChange}>
      <Dialog.Trigger
        render={<Button disabled={snapshot.status !== "ready"} />}
      >
        <Download size={16} />
        내보내기
      </Dialog.Trigger>
      <Modal
        title="스냅샷 내보내기"
        description={
          snapshot.nodes?.length
            ? `control과 compute를 포함한 ${snapshot.nodes.length}개 VM의 .msb 아카이브를 하나의 파일로 묶어 저장합니다. 다른 호스트에서 가져오기로 불러올 수 있습니다.`
            : "golden image까지 포함한 .msb 아카이브 하나로 저장합니다. 다른 호스트에서 가져오기로 불러올 수 있습니다."
        }
        closeDisabled={mutation.isPending}
      >
        <form
          onSubmit={(event) => {
            event.preventDefault();
            mutation.mutate();
          }}
        >
          <div className="modal-body space-y-5">
            <TransferMode
              value={mode}
              onChange={setMode}
              fileLabel="이 호스트의 파일"
              hasBuckets={buckets.length > 0}
              disabled={mutation.isPending}
            />
            {mode === "bucket" ? (
              <>
                <div className="field">
                  <span>버킷</span>
                  <SelectInput
                    label="버킷"
                    value={bucketId}
                    onChange={setBucketId}
                    disabled={mutation.isPending}
                    items={buckets.map((b) => ({
                      value: b.id,
                      label: `${b.id} · ${bucketLocation(b)}`,
                    }))}
                  />
                </div>
                <label className="field">
                  <span>
                    파일 이름 <span className="optional">선택</span>
                  </span>
                  <div className="input input-affix font-mono">
                    {bucket?.prefix && (
                      <span title={bucket.prefix}>{bucket.prefix}</span>
                    )}
                    <Input
                      autoFocus
                      value={name}
                      onValueChange={setName}
                      onBlur={() => setName((n) => n.replace(/\.msb$/i, ""))}
                      placeholder={snapshot.id}
                      aria-label="파일 이름"
                      disabled={mutation.isPending}
                    />
                    <span>.msb</span>
                  </div>
                  <small>
                    s3://{bucket?.name}/{key}에 올립니다. 같은 이름의 파일이
                    있으면 덮어쓰지 않습니다.
                  </small>
                </label>
                <div className="info-notice">
                  farmd 호스트에 아카이브를 잠시 만든 뒤 올리고, 다 올리면
                  지웁니다. 그동안 스냅샷 크기만큼의 디스크 공간이 필요합니다.
                </div>
              </>
            ) : (
              <label className="field">
                <span>
                  저장 경로 <span className="optional">선택</span>
                </span>
                <Input
                  autoFocus
                  value={path}
                  onValueChange={setPath}
                  className="input font-mono"
                  placeholder={defaultPath}
                  disabled={mutation.isPending}
                />
                <small>
                  farmd 호스트의 절대 경로입니다. 폴더를 지정하면 그 안에{" "}
                  {snapshot.id}.msb로 저장합니다.
                </small>
              </label>
            )}
            {mutation.error && <ErrorNotice error={mutation.error} />}
          </div>
          <div className="modal-footer justify-end">
            <Dialog.Close render={<Button disabled={mutation.isPending} />}>
              취소
            </Dialog.Close>
            <Button
              type="submit"
              variant="primary"
              disabled={mutation.isPending || (mode === "bucket" && !bucket)}
            >
              {mutation.isPending ? (
                <LoaderCircle size={16} className="animate-spin" />
              ) : mode === "bucket" ? (
                <CloudUpload size={16} />
              ) : (
                <Download size={16} />
              )}
              내보내기 시작
            </Button>
          </div>
        </form>
      </Modal>
    </Dialog.Root>
  );
}

export function ImportSnapshotDialog() {
  const client = useQueryClient();
  const buckets = useQuery(bucketsQuery()).data ?? [];
  const [mode, setMode] = useState<"file" | "bucket">("file");
  const [path, setPath] = useState("");
  const [bucketId, setBucketId] = useState("");
  const [key, setKey] = useState("");
  const [id, setId] = useState("");
  const mutation = useMutation({
    mutationFn: () =>
      api.importSnapshot({
        ...(mode === "bucket"
          ? { bucket: bucketId, key }
          : { path: path.trim() }),
        ...(id ? { id } : {}),
      }),
    onSuccess: async () => {
      await refresh(client);
      dialog.setOpen(false);
    },
  });
  const dialog = useDialog(mutation.isPending, () => {
    setMode(buckets.length ? "bucket" : "file");
    setPath("");
    setBucketId(buckets[0]?.id ?? "");
    setKey("");
    setId("");
    mutation.reset();
  });
  const objects = useQuery({
    ...bucketObjectsQuery(bucketId),
    enabled: dialog.open && mode === "bucket" && Boolean(bucketId),
  });
  const bucket = buckets.find((b) => b.id === bucketId);
  const source = mode === "bucket" ? key : path;
  const fileId = (source.split("/").pop() ?? "").replace(/\.msb$/, "");
  return (
    <Dialog.Root open={dialog.open} onOpenChange={dialog.onOpenChange}>
      <Dialog.Trigger render={<Button />}>
        <Upload size={16} />
        가져오기
      </Dialog.Trigger>
      <Modal
        title="스냅샷 가져오기"
        description="내보낸 .msb 아카이브를 이 호스트의 스냅샷으로 등록합니다."
        closeDisabled={mutation.isPending}
      >
        <form
          onSubmit={(event) => {
            event.preventDefault();
            mutation.mutate();
          }}
        >
          <div className="modal-body space-y-5">
            <TransferMode
              value={mode}
              onChange={(next) => {
                setMode(next);
                setId("");
              }}
              fileLabel="이 호스트의 파일"
              hasBuckets={buckets.length > 0}
              disabled={mutation.isPending}
            />
            {mode === "bucket" ? (
              <>
                <div className="field">
                  <span>버킷</span>
                  <SelectInput
                    label="버킷"
                    value={bucketId}
                    onChange={(next) => {
                      setBucketId(next);
                      setKey("");
                    }}
                    disabled={mutation.isPending}
                    items={buckets.map((b) => ({
                      value: b.id,
                      label: `${b.id} · ${bucketLocation(b)}`,
                    }))}
                  />
                </div>
                <div className="field">
                  <span className="flex items-center justify-between gap-2">
                    아카이브
                    <button
                      type="button"
                      className="text-link"
                      disabled={objects.isFetching}
                      onClick={() => void objects.refetch()}
                    >
                      <RefreshCw size={15} />
                      새로 고침
                    </button>
                  </span>
                  {objects.isPending ? (
                    <Loading label="버킷을 읽는 중" />
                  ) : objects.error ? (
                    <ErrorNotice error={objects.error} />
                  ) : !objects.data.length ? (
                    <p className="field-hint">
                      {bucket && bucketLocation(bucket)} 아래에 .msb 파일이
                      없습니다.
                    </p>
                  ) : (
                    <div
                      className="bucket-objects"
                      role="radiogroup"
                      aria-label="아카이브"
                    >
                      {objects.data.map((object) => {
                        const summary = archiveSummary(object.metadata);
                        return (
                          <button
                            type="button"
                            role="radio"
                            aria-checked={key === object.key}
                            key={object.key}
                            className={key === object.key ? "selected" : ""}
                            disabled={mutation.isPending}
                            onClick={() => setKey(object.key)}
                          >
                            <span className="font-mono">{object.key}</span>
                            <span
                              className={
                                object.metadata ? "" : "bucket-object-legacy"
                              }
                            >
                              {summary.title}
                            </span>
                            <small>
                              {[
                                summary.detail,
                                bytes(object.size_bytes),
                                object.modified_at &&
                                  dateTime(object.modified_at),
                              ]
                                .filter(Boolean)
                                .join(" · ")}
                            </small>
                          </button>
                        );
                      })}
                    </div>
                  )}
                </div>
              </>
            ) : (
              <label className="field">
                <span>
                  파일 경로 <span className="field-required">*</span>
                </span>
                <Input
                  autoFocus
                  value={path}
                  onValueChange={setPath}
                  className="input font-mono"
                  placeholder="/Users/me/farm-snapshots/dev-clean.msb"
                  required
                  disabled={mutation.isPending}
                />
                <small>farmd 호스트의 절대 경로</small>
              </label>
            )}
            <label className="field">
              <span>
                스냅샷 ID <span className="optional">선택</span>
              </span>
              <Input
                value={id}
                onValueChange={setId}
                className="input font-mono"
                placeholder={fileId || "파일 이름을 사용"}
                maxLength={48}
                pattern={idPattern}
                disabled={mutation.isPending}
              />
              <small>비워 두면 파일 이름을 ID로 사용합니다.</small>
            </label>
            {mode === "bucket" && (
              <div className="info-notice">
                farmd 호스트로 내려받아 등록한 뒤, 내려받은 파일은 지웁니다.
              </div>
            )}
            {mutation.error && <ErrorNotice error={mutation.error} />}
          </div>
          <div className="modal-footer justify-end">
            <Dialog.Close render={<Button disabled={mutation.isPending} />}>
              취소
            </Dialog.Close>
            <Button
              type="submit"
              variant="primary"
              disabled={mutation.isPending || !source.trim()}
            >
              {mutation.isPending ? (
                <LoaderCircle size={16} className="animate-spin" />
              ) : mode === "bucket" ? (
                <CloudDownload size={16} />
              ) : (
                <Upload size={16} />
              )}
              가져오기 시작
            </Button>
          </div>
        </form>
      </Modal>
    </Dialog.Root>
  );
}

export function DeleteSnapshotDialog({ snapshot }: { snapshot: Snapshot }) {
  const client = useQueryClient();
  const [forget, setForget] = useState(false);
  const [left, setLeft] = useState<string[]>([]);
  const mutation = useMutation({
    mutationFn: () => api.deleteSnapshot(snapshot.id, forget),
    onSuccess: async (removed) => {
      await refresh(client);
      // The snapshot is gone either way; say which hosts kept their copy.
      if (removed.copies_left?.length) setLeft(removed.copies_left);
      else dialog.setOpen(false);
    },
  });
  const dialog = useDialog(mutation.isPending, () => {
    setForget(false);
    setLeft([]);
    mutation.reset();
  });
  return (
    <Dialog.Root open={dialog.open} onOpenChange={dialog.onOpenChange}>
      <Dialog.Trigger
        render={
          <Button
            variant="ghost"
            disabled={busySnapshot(snapshot)}
            className="delete-trigger"
          />
        }
        aria-label={`${snapshot.id} 스냅샷 삭제`}
      >
        <Trash2 size={16} />
      </Dialog.Trigger>
      <Modal
        title="스냅샷을 삭제할까요?"
        description={
          forget
            ? "이 스냅샷의 coordinator 기록만 삭제합니다. 이 스냅샷으로 만든 farm에는 영향이 없습니다."
            : "이 호스트에 저장된 스냅샷 디스크를 삭제합니다. 이 스냅샷으로 만든 farm에는 영향이 없습니다."
        }
        closeDisabled={mutation.isPending}
      >
        <div className="modal-body space-y-5">
          <div className="action-context">
            <Snowflake size={20} />
            <div>
              <strong className="font-mono">{snapshot.id}</strong>
              <p>
                {snapshot.archive
                  ? "내보낸 아카이브는 삭제하지 않습니다."
                  : "내보낸 아카이브가 없습니다. 삭제하면 되돌릴 수 없습니다."}
              </p>
              {snapshot.archive && <CopyText value={snapshot.archive.path} />}
              {!forget && snapshot.copies?.length ? (
                <p>
                  {snapshot.copies.map((copy) => copy.host_id).join(", ")}에 둔
                  사본도 지웁니다.
                </p>
              ) : null}
            </div>
          </div>
          <ForgetOption
            checked={forget}
            onChange={setForget}
            disabled={mutation.isPending}
            left="스냅샷 디스크"
          />
          {mutation.error && <ErrorNotice error={mutation.error} />}
          {left.length > 0 && (
            <div className="warning-notice">
              스냅샷은 삭제했지만 다음 호스트의 사본은 지우지 못했습니다. 그
              호스트에서 직접 지우세요.
              <ul className="mt-1.5 font-mono text-xs">
                {left.map((line) => (
                  <li key={line}>{line}</li>
                ))}
              </ul>
            </div>
          )}
        </div>
        <div className="modal-footer justify-end">
          <Dialog.Close render={<Button disabled={mutation.isPending} />}>
            {left.length ? "닫기" : "취소"}
          </Dialog.Close>
          <Button
            variant="danger"
            disabled={mutation.isPending || left.length > 0}
            onClick={() => mutation.mutate()}
          >
            {mutation.isPending ? (
              <LoaderCircle size={16} className="animate-spin" />
            ) : (
              <Trash2 size={16} />
            )}
            {forget ? "기록만 삭제" : "스냅샷 삭제"}
          </Button>
        </div>
      </Modal>
    </Dialog.Root>
  );
}
