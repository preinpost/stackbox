import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { Cloud, Cpu, LoaderCircle, Snowflake } from "lucide-react";
import {
  activeJob,
  farmsQuery,
  hostQuery,
  jobsQuery,
  snapshotsQuery,
  type Snapshot,
} from "../api/client";
import { BucketsPanel } from "../components/bucket-actions";
import { StepProgress } from "../components/job-steps";
import {
  DeleteSnapshotDialog,
  ExportSnapshotDialog,
  ImportSnapshotDialog,
  RestoreSnapshotDialog,
} from "../components/snapshot-actions";
import {
  CopyText,
  EmptyState,
  ErrorNotice,
  Loading,
  PageHeading,
  StatusBadge,
} from "../components/ui";
import { bytes, componentLabel, dateTime, memory, snapshotHost } from "../lib";

export function SnapshotsPage() {
  const snapshots = useQuery(snapshotsQuery());
  const host = useQuery(hostQuery());
  const sorted = [...(snapshots.data ?? [])].sort(
    (a, b) => b.created_at - a.created_at,
  );
  return (
    <>
      <PageHeading
        eyebrow="FROZEN ENVIRONMENTS"
        title="Snapshots"
        description="설치가 끝난 farm을 얼려두고, 같은 환경을 다시 설치하지 않고 새 farm으로 불러오세요. S3 버킷으로 올려 다른 호스트로 옮길 수도 있습니다."
        actions={<ImportSnapshotDialog />}
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
            <h2>모든 스냅샷</h2>
            <span className="count-pill">{sorted.length}</span>
          </div>
        </div>
        {snapshots.isPending ? (
          <Loading label="스냅샷을 불러오는 중" />
        ) : !sorted.length ? (
          <EmptyState
            icon={<Snowflake size={24} />}
            title="얼려둔 환경이 없습니다"
            description="Farm 상세 화면의 얼리기로 배포가 끝난 VM을 저장하거나, 내보낸 .msb 파일을 파일 경로나 S3 버킷에서 가져오세요."
          />
        ) : (
          sorted.map((snapshot) => (
            <SnapshotRow
              snapshot={snapshot}
              localArch={host.data?.arch}
              key={snapshot.id}
            />
          ))
        )}
      </section>
      <BucketsPanel />
    </>
  );
}

/** Progress of an export to, or import from, a bucket. */
function TransferProgress({ snapshot }: { snapshot: Snapshot }) {
  const transfer = snapshot.transfer;
  if (!transfer) return null;
  const exporting = snapshot.status === "exporting";
  const total = transfer.total_bytes;
  const percent = total ? Math.floor((transfer.done_bytes / total) * 100) : 0;
  // No byte count yet, or the bytes are in and the host is still registering them.
  const waiting = total == null || (transfer.done_bytes >= total && !exporting);
  const label =
    total == null
      ? exporting
        ? "디스크를 아카이브로 묶는 중"
        : "S3에 연결하는 중"
      : transfer.done_bytes >= total && !exporting
        ? "내려받음 · 스냅샷 등록 중"
        : `${exporting ? "S3에 올리는 중" : "S3에서 내려받는 중"} · ${percent}% (${bytes(transfer.done_bytes)} / ${bytes(total)})`;
  return (
    <div className="mt-2.5 max-w-xl">
      <div className="job-meta mt-0 flex-wrap">
        {waiting ? (
          <LoaderCircle size={13} className="animate-spin" />
        ) : (
          <Cloud size={13} />
        )}
        <span>{label}</span>
        <span>·</span>
        <span className="font-mono">
          {transfer.bucket}:{transfer.key}
        </span>
      </div>
      {total != null && (
        <div className="meter mt-2" aria-hidden>
          <span style={{ width: `${percent}%` }} />
        </div>
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
  const farms = useQuery(farmsQuery());
  // Architecture alone decides whether the guest can boot here; the label says where it came from.
  const foreignArch = Boolean(localArch && snapshot.arch !== localArch);
  const sourceExists = farms.data?.some((f) => f.id === snapshot.source_farm);
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
  return (
    <div className="job-row snapshot-row">
      <span className={`job-kind-icon ${snapshot.status}`}>
        <Snowflake size={16} />
      </span>
      <div className="job-row-main">
        <div className="flex flex-wrap items-center gap-2.5">
          <strong className="font-mono text-stone-800">{snapshot.id}</strong>
          <StatusBadge status={snapshot.status} />
          {known && (
            <span className="muted text-xs">
              {snapshot.template
                ? `${snapshot.template === "control" ? "Control" : "Compute"} 템플릿${snapshot.release ? ` · OpenStack ${snapshot.release}` : ""}`
                : snapshot.deployed
                  ? "OpenStack 배포 완료"
                  : "배포 전 VM"}
            </span>
          )}
          <span
            className={`snapshot-host ${foreignArch ? "snapshot-host-foreign" : ""}`}
            title={
              foreignArch
                ? `이 호스트는 ${localArch}이므로 복원할 수 없습니다`
                : "이 스냅샷을 얼린 호스트"
            }
          >
            <Cpu size={12} />
            {snapshotHost(snapshot)}
          </span>
        </div>
        {freezing && (
          <div className="mt-1.5">
            <StepProgress job={freezing} />
          </div>
        )}
        {snapshot.note && <p className="mt-1.5">{snapshot.note}</p>}
        {known && (
          <div className="service-tags mt-2.5">
            {snapshot.plan.components.map((c) => (
              <span key={c}>{componentLabel(c)}</span>
            ))}
          </div>
        )}
        <div className="job-meta flex-wrap">
          <span>
            {snapshot.source_farm ? (
              sourceExists ? (
                <Link
                  to="/farms/$farmId"
                  params={{ farmId: snapshot.source_farm }}
                  className="hover:underline"
                >
                  {snapshot.source_farm}
                </Link>
              ) : (
                snapshot.source_farm
              )
            ) : (
              "가져온 파일"
            )}
            에서 생성
          </span>
          <span>·</span>
          <span>{dateTime(snapshot.created_at)}</span>
          {known && (
            <>
              <span>·</span>
              <span>VM 메모리 {memory(snapshot.plan.memory_mib)}</span>
            </>
          )}
        </div>
        <TransferProgress snapshot={snapshot} />
        {snapshot.archive && (
          <div className="job-meta flex-wrap">
            <span>
              {snapshot.archive.bucket
                ? `S3(${snapshot.archive.bucket})에 올림`
                : "내보낸 파일"}{" "}
              · {bytes(snapshot.archive.size_bytes)} ·{" "}
              {dateTime(snapshot.archive.exported_at)}
            </span>
            <CopyText value={snapshot.archive.path} />
          </div>
        )}
        {snapshot.error && <p className="job-error">{snapshot.error}</p>}
      </div>
      <div className="flex shrink-0 flex-wrap items-center gap-2">
        {snapshot.template !== "compute" && (
          <RestoreSnapshotDialog
            snapshot={snapshot}
            foreignArch={foreignArch}
          />
        )}
        <ExportSnapshotDialog snapshot={snapshot} />
        <DeleteSnapshotDialog snapshot={snapshot} />
      </div>
    </div>
  );
}
