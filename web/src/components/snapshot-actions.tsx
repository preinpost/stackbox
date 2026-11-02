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
  bytes,
  componentLabel,
  dateTime,
  hostNotice,
  hostReady,
  isAllInOneFarm,
  memory,
  snapshotHost,
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

/** Snapshots live with this machine's host, so only its farms can be frozen. */
export const canFreeze = (farm: Farm, localHostId?: string) =>
  Boolean(localHostId) &&
  farm.host_id === localHostId &&
  !farm.active_job &&
  Boolean(farm.plan && farm.exposure) &&
  isAllInOneFarm(farm) &&
  ["created", "ready", "failed"].includes(farm.status) &&
  farm.nodes.length === 1 &&
  farm.nodes[0].status === "running";

export function FreezeFarmDialog({ farm }: { farm: Farm }) {
  const client = useQueryClient();
  const host = useQuery(hostQuery());
  const localHost = useQuery(hostsQuery()).data?.find(
    (h) => h.kind === "local",
  );
  const snapshots = useQuery(snapshotsQuery());
  const suggested = `${farm.id.slice(0, 42)}-${farm.status === "ready" ? "clean" : "base"}`;
  const [id, setId] = useState(suggested);
  const [note, setNote] = useState("");
  const mutation = useMutation({
    mutationFn: () =>
      api.freeze(farm.id, {
        id,
        ...(note.trim() ? { note: note.trim() } : {}),
      }),
    onSuccess: async () => {
      await refresh(client);
      dialog.setOpen(false);
    },
  });
  const dialog = useDialog(mutation.isPending, () => {
    setId(suggested);
    setNote("");
    mutation.reset();
  });
  const duplicate = snapshots.data?.some((s) => s.id === id);
  return (
    <Dialog.Root open={dialog.open} onOpenChange={dialog.onOpenChange}>
      <Dialog.Trigger
        render={<Button disabled={!canFreeze(farm, localHost?.id)} />}
        title={
          farm.host_id !== localHost?.id
            ? "스냅샷은 로컬 호스트의 farm만 만들 수 있습니다."
            : "VM을 스냅샷으로 얼려둡니다."
        }
      >
        <Snowflake size={14} />
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
                <strong className="font-mono">{farm.id}</strong>
                <p>
                  {farm.status === "ready"
                    ? "배포된 OpenStack을 그대로 저장합니다. 복원한 farm은 바로 Ready가 됩니다."
                    : "OpenStack 배포 전 상태입니다. 복원한 farm은 VM 준비됨 상태로 시작합니다."}
                </p>
              </div>
            </div>
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
                <small className="text-red-600">
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
            <div className="warning-notice">
              데이터베이스를 깨끗하게 저장하기 위해 VM을 잠시 정지했다가 다시
              시작합니다. 서비스가 다시 응답할 때까지 몇 분 정도 걸립니다. 이
              스냅샷은{" "}
              {snapshotHost({
                arch: host.data?.arch ?? undefined,
                platform: host.data?.os,
              })}
              에서만 복원할 수 있습니다.
            </div>
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
                <LoaderCircle size={15} className="animate-spin" />
              ) : (
                <Snowflake size={15} />
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
  // A snapshot restores on this machine, where it is kept.
  const localHost = useQuery(hostsQuery()).data?.find(
    (h) => h.kind === "local",
  );
  const [id, setId] = useState("");
  const [owner, setOwner] = useState("");
  const [hostName, setHostName] = useState("localhost");
  const mutation = useMutation({
    mutationFn: () =>
      api.create({
        id,
        host_id: localHost?.id,
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
    setId("");
    setOwner("");
    setHostName("localhost");
    mutation.reset();
  });
  const duplicate = farms.data?.some((f) => f.id === id);
  const capacity = localHost?.capacity;
  const remaining = capacity
    ? capacity.memory_budget_mib - capacity.memory_reserved_mib
    : null;
  const hostMissing = !localHost || !hostReady(localHost);
  const overBudget = remaining !== null && snapshot.plan.memory_mib > remaining;
  const ready =
    ["ready", "exporting"].includes(snapshot.status) && !foreignArch;
  return (
    <Dialog.Root open={dialog.open} onOpenChange={dialog.onOpenChange}>
      <Dialog.Trigger render={<Button variant="primary" disabled={!ready} />}>
        <CopyPlus size={14} />
        Farm 만들기
      </Dialog.Trigger>
      <Modal
        title="스냅샷에서 farm 만들기"
        description="얼려둔 VM을 복사해 새 farm으로 시작합니다. OpenStack을 다시 설치하지 않습니다."
        closeDisabled={mutation.isPending}
      >
        <form
          onSubmit={(event) => {
            event.preventDefault();
            if (!duplicate && !overBudget && !hostMissing) mutation.mutate();
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
                  {snapshot.deployed ? " · 배포 완료" : " · 배포 전"}
                </p>
                <p>
                  {snapshotHost(snapshot)}에서 얼린 게스트 · 이 호스트의
                  아키텍처와 같아야 복원됩니다
                </p>
              </div>
            </div>
            <div className="grid gap-4 sm:grid-cols-2">
              <label className="field">
                <span>
                  Farm ID <span className="text-emerald-600">*</span>
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
              </label>
            </div>
            <label className="field">
              <span>외부 접속 호스트</span>
              <Input
                value={hostName}
                onValueChange={setHostName}
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
            {foreignArch && (
              <div className="warning-notice">
                이 스냅샷은 다른 아키텍처({snapshot.arch})에서 얼렸습니다. 이
                호스트에서는 복원할 수 없습니다.
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
                {localHost
                  ? hostNotice(localHost)
                  : "스냅샷은 로컬 호스트로 복원합니다. Hosts에서 로컬 호스트를 등록하고 환경 준비를 마치세요."}
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
              disabled={
                mutation.isPending || duplicate || overBudget || hostMissing
              }
            >
              {mutation.isPending ? (
                <LoaderCircle size={15} className="animate-spin" />
              ) : (
                <CopyPlus size={15} />
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
          { value: "bucket", label: "S3 버킷", icon: <Cloud size={14} /> },
          { value: "file", label: fileLabel, icon: <HardDrive size={14} /> },
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
        <Download size={14} />
        내보내기
      </Dialog.Trigger>
      <Modal
        title="스냅샷 내보내기"
        description="golden image까지 포함한 .msb 아카이브 하나로 저장합니다. 다른 호스트에서 가져오기로 불러올 수 있습니다."
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
                <LoaderCircle size={15} className="animate-spin" />
              ) : mode === "bucket" ? (
                <CloudUpload size={15} />
              ) : (
                <Download size={15} />
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
        <Upload size={14} />
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
                      <RefreshCw size={13} />
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
                      {objects.data.map((object) => (
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
                          <small>
                            {bytes(object.size_bytes)}
                            {object.modified_at
                              ? ` · ${dateTime(object.modified_at)}`
                              : ""}
                          </small>
                        </button>
                      ))}
                    </div>
                  )}
                </div>
              </>
            ) : (
              <label className="field">
                <span>
                  파일 경로 <span className="text-emerald-600">*</span>
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
                <LoaderCircle size={15} className="animate-spin" />
              ) : mode === "bucket" ? (
                <CloudDownload size={15} />
              ) : (
                <Upload size={15} />
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
  const mutation = useMutation({
    mutationFn: () => api.deleteSnapshot(snapshot.id, forget),
    onSuccess: async () => {
      await refresh(client);
      dialog.setOpen(false);
    },
  });
  const dialog = useDialog(mutation.isPending, () => {
    setForget(false);
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
            </div>
          </div>
          <ForgetOption
            checked={forget}
            onChange={setForget}
            disabled={mutation.isPending}
            left="스냅샷 디스크"
          />
          {mutation.error && <ErrorNotice error={mutation.error} />}
        </div>
        <div className="modal-footer justify-end">
          <Dialog.Close render={<Button disabled={mutation.isPending} />}>
            취소
          </Dialog.Close>
          <Button
            variant="danger"
            disabled={mutation.isPending}
            onClick={() => mutation.mutate()}
          >
            {mutation.isPending ? (
              <LoaderCircle size={15} className="animate-spin" />
            ) : (
              <Trash2 size={15} />
            )}
            {forget ? "기록만 삭제" : "스냅샷 삭제"}
          </Button>
        </div>
      </Modal>
    </Dialog.Root>
  );
}
