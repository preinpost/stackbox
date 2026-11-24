import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { Cloud, HardDrive, LoaderCircle, Snowflake } from "lucide-react";
import { useEffect, useState } from "react";
import {
  activeJob,
  stackboxesQuery,
  hostQuery,
  hostsQuery,
  jobsQuery,
  snapshotsQuery,
  type Snapshot,
} from "../api/client";
import { BucketsPanel } from "../components/bucket-actions";
import { StepProgress } from "../components/job-steps";
import {
  CreateFromBucketDialog,
  DeleteSnapshotDialog,
  ExportSnapshotDialog,
  ImportSnapshotDialog,
  RestoreSnapshotDialog,
} from "../components/snapshot-actions";
import { SnapshotCopies } from "../components/snapshot-copies";
import {
  CopyText,
  EmptyState,
  ErrorNotice,
  Loading,
  PageHeading,
  StatusBadge,
} from "../components/ui";
import {
  bytes,
  componentLabel,
  dateTime,
  duration,
  memory,
  snapshotHost,
  snapshotOrigin,
  transferRate,
} from "../lib";

/** The mock shows four services and folds the rest into a count. */
const tagLimit = 4;

export function SnapshotsPage() {
  const snapshots = useQuery(snapshotsQuery());
  const host = useQuery(hostQuery());
  const sorted = [...(snapshots.data ?? [])].sort(
    (a, b) => b.created_at - a.created_at,
  );
  const archived = sorted.filter((snapshot) => snapshot.archive).length;
  return (
    <>
      <PageHeading
        eyebrow="FROZEN ENVIRONMENTS"
        title="Snapshots"
        description="설치가 끝난 stackbox를 얼려두고, 같은 환경을 다시 설치하지 않고 새 stackbox로 불러오세요. S3 버킷으로 올려 다른 호스트로 옮길 수도 있습니다."
        actions={
          <div className="flex items-center gap-2">
            <CreateFromBucketDialog triggerVariant="primary" />
            <ImportSnapshotDialog />
          </div>
        }
      />
      {snapshots.error && (
        <ErrorNotice
          error={snapshots.error}
          retry={() => {
            void snapshots.refetch();
          }}
        />
      )}
      <section className="panel" aria-label="스냅샷 목록">
        <div className="panel-toolbar">
          <div className="flex items-center gap-2.5">
            <h2 className="flex items-center gap-2">
              <Snowflake size={16} />
              모든 스냅샷
            </h2>
            <span className="count-pill">{sorted.length}</span>
          </div>
          <span className="field-hint">
            복원은 같은 아키텍처의 호스트에서만 가능합니다
          </span>
        </div>
        {snapshots.isPending ? (
          <Loading label="스냅샷을 불러오는 중" />
        ) : !sorted.length ? (
          <EmptyState
            icon={<Snowflake size={24} />}
            title="얼려둔 환경이 없습니다"
            description="Stackbox 상세 화면의 얼리기로 배포가 끝난 VM을 저장하거나, 내보낸 .msb 파일을 파일 경로나 S3 버킷에서 가져오세요."
          />
        ) : (
          <>
            {sorted.map((snapshot) => (
              <SnapshotRow
                snapshot={snapshot}
                localArch={host.data?.arch}
                key={snapshot.id}
              />
            ))}
            <div className="panel-foot">
              <span>
                스냅샷은 같은 아키텍처의 호스트에서만 부팅됩니다. 복원할 때 새
                stackbox가 만들어집니다.
              </span>
              <span className="chip">아카이브 {archived}개</span>
            </div>
          </>
        )}
      </section>
      <BucketsPanel />
    </>
  );
}

/** Sliding window of observed `(done_bytes, time_ms)` samples for one transfer. */
type TransferSample = { done: number; at: number };
type TransferTracker = { id: string; samples: TransferSample[] };

/** Average speed (bytes/sec) across the observed window, or `null` until progress advances. */
export function transferSpeed(samples: TransferSample[]): number | null {
  if (samples.length < 2) return null;
  const first = samples[0];
  const last = samples[samples.length - 1];
  const deltaBytes = last.done - first.done;
  const deltaSec = (last.at - first.at) / 1000;
  if (deltaBytes <= 0 || deltaSec <= 0) return null;
  return deltaBytes / deltaSec;
}

/** Progress of an export to, or import from, a bucket. */
export function TransferProgress({ snapshot }: { snapshot: Snapshot }) {
  const transfer = snapshot.transfer;
  const transferKey = transfer
    ? `${snapshot.status}:${transfer.bucket}:${transfer.key}`
    : "";
  const doneBytes = transfer?.done_bytes ?? 0;
  const totalBytes = transfer?.total_bytes ?? null;
  const [tracker, setTracker] = useState<TransferTracker>({
    id: transferKey,
    samples: [],
  });
  useEffect(() => {
    if (!transferKey || totalBytes == null) {
      setTracker({ id: "", samples: [] });
      return;
    }
    const now = Date.now();
    // Keep roughly the last 60s of samples so speed reacts smoothly to bursts and slowdowns.
    const windowMs = 60_000;
    setTracker((prev) => {
      if (prev.id !== transferKey) {
        return {
          id: transferKey,
          samples: [{ done: doneBytes, at: now }],
        };
      }
      const last = prev.samples[prev.samples.length - 1];
      if (last && last.done === doneBytes) return prev;
      if (last && doneBytes < last.done) {
        return {
          id: transferKey,
          samples: [{ done: doneBytes, at: now }],
        };
      }
      const next = [...prev.samples, { done: doneBytes, at: now }];
      // Retain at least two points even when a single 64 MiB part takes over a minute.
      const trimmed = next.filter(
        (sample, idx) =>
          idx >= next.length - 2 || now - sample.at <= windowMs,
      );
      return { id: transferKey, samples: trimmed };
    });
  }, [transferKey, doneBytes, totalBytes]);

  if (!transfer) return null;
  const exporting = snapshot.status === "exporting";
  const total = transfer.total_bytes;
  const percent = total ? Math.floor((transfer.done_bytes / total) * 100) : 0;
  // No byte count yet, or the bytes are in and the host is still registering them.
  const waiting = total == null || (transfer.done_bytes >= total && !exporting);
  const speed =
    tracker.id === transferKey ? transferSpeed(tracker.samples) : null;
  const remainingBytes = total != null ? Math.max(0, total - transfer.done_bytes) : 0;
  const eta =
    speed && remainingBytes > 0
      ? duration(Math.ceil(remainingBytes / speed))
      : null;
  const stats = [
    `${bytes(transfer.done_bytes)} / ${bytes(total ?? 0)}`,
    speed ? transferRate(speed) : null,
    eta ? `약 ${eta} 남음` : null,
  ]
    .filter(Boolean)
    .join(" · ");
  const label =
    total == null
      ? exporting
        ? "디스크를 아카이브로 묶는 중"
        : "S3에 연결하는 중"
      : transfer.done_bytes >= total && !exporting
        ? "내려받음 · 스냅샷 등록 중"
        : `${exporting ? "S3에 올리는 중" : "S3에서 내려받는 중"} · ${percent}% (${stats})`;
  return (
    <div className="snap-progress">
      <span className="cap">
        {waiting ? (
          <LoaderCircle size={14} className="animate-spin" />
        ) : (
          <Cloud size={14} />
        )}
        {label}
        <code>
          {transfer.bucket}:{transfer.key}
        </code>
      </span>
      {total != null && (
        <span
          className="bar"
          role="progressbar"
          aria-valuenow={percent}
          aria-valuemin={0}
          aria-valuemax={100}
          aria-label={`${exporting ? "S3 업로드" : "S3 다운로드"} ${percent}%`}
        >
          <i style={{ width: `${percent}%` }} />
        </span>
      )}
    </div>
  );
}

function SnapshotRow({
  snapshot,
  localArch,
}: {
  snapshot: Snapshot;
  localArch?: string;
}) {
  const stackboxes = useQuery(stackboxesQuery());
  const hosts = useQuery(hostsQuery());
  // The host that froze a snapshot keeps it and boots it; one imported here must match this
  // machine's architecture. The label says where it came from.
  const origin = snapshotOrigin(snapshot, hosts.data);
  const originRegistered = hosts.data?.some((h) => h.id === origin);
  const foreignArch = Boolean(
    !snapshot.host_id && localArch && snapshot.arch !== localArch,
  );
  const sourceExists = stackboxes.data?.some((f) => f.id === snapshot.source_stackbox);
  // An import learns its services from the archive only when it completes.
  const known = snapshot.status !== "importing" && snapshot.status !== "failed";
  const creating = snapshot.status === "creating";
  const jobs = useQuery({ ...jobsQuery(), enabled: creating });
  const freezing = creating
    ? jobs.data?.find(
        (j) =>
          j.kind === "snapshot" &&
          j.snapshot_id === snapshot.id &&
          activeJob(j),
      )
    : undefined;
  const components = known ? snapshot.plan.components : [];
  const shown = components.slice(0, tagLimit);
  return (
    <article className="snap">
      <span className="snap-glyph" aria-hidden>
        <Snowflake size={16} />
      </span>
      <div className="snap-body">
        <div className="snap-title">
          <strong>{snapshot.id}</strong>
          <StatusBadge status={snapshot.status} />
          {known && (
            <span className={snapshot.deployed ? "chip accent" : "chip"}>
              {snapshot.template
                ? `${snapshot.template === "control" ? "Control" : "Compute"} 템플릿`
                : snapshot.deployed
                  ? "OpenStack 배포 완료"
                  : "배포 전 VM"}
            </span>
          )}
          {snapshot.release && <span className="chip">{snapshot.release}</span>}
        </div>
        <div className="snap-meta">
          <span>
            원본{" "}
            {snapshot.source_stackbox ? (
              sourceExists ? (
                <Link
                  to="/stackboxes/$stackboxId"
                  params={{ stackboxId: snapshot.source_stackbox }}
                >
                  {snapshot.source_stackbox}
                </Link>
              ) : (
                snapshot.source_stackbox
              )
            ) : (
              "가져온 파일"
            )}
          </span>
          <span className="sep">·</span>
          <span
            className={foreignArch ? "snap-flag" : undefined}
            title={
              foreignArch
                ? `이 호스트는 ${localArch}이므로 복원할 수 없습니다`
                : "이 스냅샷을 얼린 호스트"
            }
          >
            {snapshotHost(snapshot)}
          </span>
          {hosts.data && (
            <>
              <span className="sep">·</span>
              <span
                className={originRegistered ? undefined : "snap-flag"}
                title={
                  originRegistered
                    ? "원본 스냅샷을 보관하는 호스트"
                    : "원본을 보관하던 호스트가 등록되어 있지 않습니다. 다시 등록할 때까지 다른 호스트의 사본으로만 복원할 수 있습니다"
                }
              >
                <code>{origin ?? snapshot.host_id ?? "로컬 호스트"}</code>에
                보관
              </span>
            </>
          )}
          <span className="sep">·</span>
          <span>{dateTime(snapshot.created_at)}</span>
          {known && (
            <>
              <span className="sep">·</span>
              <span>VM 메모리 {memory(snapshot.plan.memory_mib)}</span>
            </>
          )}
        </div>
        {freezing && <StepProgress job={freezing} />}
        {snapshot.note && <p className="snap-note">{snapshot.note}</p>}
        {shown.length > 0 && (
          <div className="tags snap-tags">
            {shown.map((c) => (
              <span className="tag" key={c}>
                {componentLabel(c)}
              </span>
            ))}
            {components.length > shown.length && (
              <span className="tag more">
                +{components.length - shown.length}
              </span>
            )}
          </div>
        )}
        <TransferProgress snapshot={snapshot} />
        <SnapshotCopies snapshot={snapshot} />
        {snapshot.archive && (
          <div className="snap-archive">
            <HardDrive size={14} />
            <code>{snapshot.archive.path}</code>
            <span>· {bytes(snapshot.archive.size_bytes)}</span>
            <span>
              ·{" "}
              {snapshot.archive.bucket
                ? `S3(${snapshot.archive.bucket})에 올림`
                : "내보낸 파일"}
            </span>
            <span>· {dateTime(snapshot.archive.exported_at)}</span>
            <CopyText
              value={snapshot.archive.path}
              hideValue
              name="아카이브 경로"
            />
          </div>
        )}
        {snapshot.error && <p className="job-error">{snapshot.error}</p>}
      </div>
      <div className="snap-actions">
        {snapshot.template !== "compute" && (
          <RestoreSnapshotDialog
            snapshot={snapshot}
            foreignArch={foreignArch}
          />
        )}
        <ExportSnapshotDialog snapshot={snapshot} />
        <DeleteSnapshotDialog snapshot={snapshot} />
      </div>
    </article>
  );
}
