import { useMutation, useQueryClient } from "@tanstack/react-query";
import { HardDrive, LoaderCircle, X } from "lucide-react";
import { api, busySnapshot, keys, type Snapshot } from "../api/client";
import { bytes, dateTime } from "../lib";
import { ErrorNotice } from "./ui";

type Copy = NonNullable<Snapshot["copies"]>[number];

const via: Record<Copy["via"], string> = {
  bucket: "버킷에서 받음",
  stream: "coordinator가 보냄",
  found: "이미 있던 사본",
};

/** Hosts that keep a copy of the snapshot, each removable on its own. */
export function SnapshotCopies({ snapshot }: { snapshot: Snapshot }) {
  const client = useQueryClient();
  const mutation = useMutation({
    mutationFn: (hostId: string) => api.removeSnapshotCopy(snapshot.id, hostId),
    onSuccess: () => client.invalidateQueries({ queryKey: keys.snapshots }),
  });
  const copies = snapshot.copies ?? [];
  if (!copies.length) return null;
  return (
    <div className="mt-2.5">
      <div className="job-meta mt-0 flex-wrap" aria-label="사본이 있는 호스트">
        <span>사본</span>
        {copies.map((copy) => (
          <span
            key={copy.host_id}
            className="snapshot-host"
            title={`${via[copy.via]} · ${dateTime(copy.copied_at)}${copy.size_bytes ? ` · ${bytes(copy.size_bytes)}` : ""}`}
          >
            <HardDrive size={14} />
            {copy.host_id}
            <button
              type="button"
              className="snapshot-copy-remove"
              aria-label={`${copy.host_id}의 ${snapshot.id} 사본 삭제`}
              title="이 호스트의 사본만 지웁니다. 다음에 쓰면 다시 복사합니다."
              disabled={mutation.isPending || busySnapshot(snapshot)}
              onClick={() => mutation.mutate(copy.host_id)}
            >
              {mutation.isPending && mutation.variables === copy.host_id ? (
                <LoaderCircle size={14} className="animate-spin" />
              ) : (
                <X size={14} />
              )}
            </button>
          </span>
        ))}
      </div>
      {mutation.error && <ErrorNotice error={mutation.error} />}
    </div>
  );
}
