import { Dialog } from "@base-ui/react/dialog";
import { Input } from "@base-ui/react/input";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import {
  CopyPlus,
  Download,
  LoaderCircle,
  Snowflake,
  Trash2,
  Upload,
} from "lucide-react";
import { useState } from "react";
import {
  api,
  busySnapshot,
  farmsQuery,
  hostQuery,
  keys,
  snapshotsQuery,
  type Farm,
  type Snapshot,
} from "../api/client";
import { componentLabel, isAllInOneFarm, memory } from "../lib";
import { Button, CopyText, ErrorNotice, Modal } from "./ui";

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

export const canFreeze = (farm: Farm) =>
  !farm.host_id &&
  !farm.active_job &&
  Boolean(farm.plan && farm.exposure) &&
  isAllInOneFarm(farm) &&
  ["created", "ready", "failed"].includes(farm.status) &&
  farm.nodes.length === 1 &&
  farm.nodes[0].status === "running";

export function FreezeFarmDialog({ farm }: { farm: Farm }) {
  const client = useQueryClient();
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
        render={<Button disabled={!canFreeze(farm)} />}
        title={
          farm.host_id
            ? "스냅샷은 로컬 farm에서만 만들 수 있습니다."
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
              시작합니다. 서비스가 다시 응답할 때까지 몇 분 정도 걸립니다.
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

export function RestoreSnapshotDialog({ snapshot }: { snapshot: Snapshot }) {
  const client = useQueryClient();
  const navigate = useNavigate();
  const farms = useQuery(farmsQuery());
  const host = useQuery(hostQuery());
  const [id, setId] = useState("");
  const [owner, setOwner] = useState("");
  const [hostName, setHostName] = useState("localhost");
  const mutation = useMutation({
    mutationFn: () =>
      api.create({
        id,
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
  const remaining = host.data
    ? host.data.memory_budget_mib - host.data.memory_reserved_mib
    : null;
  const overBudget = remaining !== null && snapshot.plan.memory_mib > remaining;
  const ready = ["ready", "exporting"].includes(snapshot.status);
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
            if (!duplicate && !overBudget) mutation.mutate();
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
            {overBudget && (
              <div className="warning-notice">
                예약 가능한 메모리가 부족합니다. 현재 여유:{" "}
                {memory(Math.max(0, remaining ?? 0))}.
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
              disabled={mutation.isPending || duplicate || overBudget}
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

export function ExportSnapshotDialog({ snapshot }: { snapshot: Snapshot }) {
  const client = useQueryClient();
  const host = useQuery(hostQuery());
  const [path, setPath] = useState("");
  const mutation = useMutation({
    mutationFn: () => api.exportSnapshot(snapshot.id, path.trim() || undefined),
    onSuccess: async () => {
      await refresh(client);
      dialog.setOpen(false);
    },
  });
  const dialog = useDialog(mutation.isPending, () => {
    setPath("");
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
        description="golden image까지 포함한 .msb 파일 하나로 저장합니다. 다른 호스트에서 가져오기로 불러올 수 있습니다."
        closeDisabled={mutation.isPending}
      >
        <form
          onSubmit={(event) => {
            event.preventDefault();
            mutation.mutate();
          }}
        >
          <div className="modal-body space-y-5">
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
  const [path, setPath] = useState("");
  const [id, setId] = useState("");
  const mutation = useMutation({
    mutationFn: () =>
      api.importSnapshot({ path: path.trim(), ...(id ? { id } : {}) }),
    onSuccess: async () => {
      await refresh(client);
      dialog.setOpen(false);
    },
  });
  const dialog = useDialog(mutation.isPending, () => {
    setPath("");
    setId("");
    mutation.reset();
  });
  const fileId = (path.split("/").pop() ?? "").replace(/\.msb$/, "");
  return (
    <Dialog.Root open={dialog.open} onOpenChange={dialog.onOpenChange}>
      <Dialog.Trigger render={<Button />}>
        <Upload size={14} />
        가져오기
      </Dialog.Trigger>
      <Modal
        title="스냅샷 가져오기"
        description="내보낸 .msb 파일을 이 호스트의 스냅샷으로 등록합니다."
        closeDisabled={mutation.isPending}
      >
        <form
          onSubmit={(event) => {
            event.preventDefault();
            mutation.mutate();
          }}
        >
          <div className="modal-body space-y-5">
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
            {mutation.error && <ErrorNotice error={mutation.error} />}
          </div>
          <div className="modal-footer justify-end">
            <Dialog.Close render={<Button disabled={mutation.isPending} />}>
              취소
            </Dialog.Close>
            <Button
              type="submit"
              variant="primary"
              disabled={mutation.isPending || !path.trim()}
            >
              {mutation.isPending ? (
                <LoaderCircle size={15} className="animate-spin" />
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
  const mutation = useMutation({
    mutationFn: () => api.deleteSnapshot(snapshot.id),
    onSuccess: async () => {
      await refresh(client);
      dialog.setOpen(false);
    },
  });
  const dialog = useDialog(mutation.isPending, () => mutation.reset());
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
        description="이 호스트에 저장된 스냅샷 디스크를 삭제합니다. 이 스냅샷으로 만든 farm에는 영향이 없습니다."
        closeDisabled={mutation.isPending}
      >
        <div className="modal-body space-y-5">
          <div className="action-context">
            <Snowflake size={20} />
            <div>
              <strong className="font-mono">{snapshot.id}</strong>
              <p>
                {snapshot.archive
                  ? "내보낸 파일은 삭제하지 않습니다."
                  : "내보낸 파일이 없습니다. 삭제하면 되돌릴 수 없습니다."}
              </p>
              {snapshot.archive && <CopyText value={snapshot.archive.path} />}
            </div>
          </div>
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
            스냅샷 삭제
          </Button>
        </div>
      </Modal>
    </Dialog.Root>
  );
}
