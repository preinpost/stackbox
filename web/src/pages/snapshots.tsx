import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { Snowflake } from "lucide-react";
import { farmsQuery, snapshotsQuery, type Snapshot } from "../api/client";
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
import { bytes, componentLabel, dateTime, memory } from "../lib";

export function SnapshotsPage() {
  const snapshots = useQuery(snapshotsQuery());
  const sorted = [...(snapshots.data ?? [])].sort(
    (a, b) => b.created_at - a.created_at,
  );
  return (
    <>
      <PageHeading
        eyebrow="FROZEN ENVIRONMENTS"
        title="Snapshots"
        description="설치가 끝난 farm을 얼려두고, 같은 환경을 다시 설치하지 않고 새 farm으로 불러오세요."
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
            description="Farm 상세 화면의 얼리기로 배포가 끝난 VM을 저장하거나, 내보낸 .msb 파일을 가져오세요."
          />
        ) : (
          sorted.map((snapshot) => (
            <SnapshotRow snapshot={snapshot} key={snapshot.id} />
          ))
        )}
      </section>
    </>
  );
}

function SnapshotRow({ snapshot }: { snapshot: Snapshot }) {
  const farms = useQuery(farmsQuery());
  const sourceExists = farms.data?.some((f) => f.id === snapshot.source_farm);
  // An import learns its services from the archive only when it completes.
  const known = snapshot.status !== "importing" && snapshot.status !== "failed";
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
              {snapshot.deployed ? "OpenStack 배포 완료" : "배포 전 VM"}
            </span>
          )}
          <span className="muted text-xs">{snapshot.arch}</span>
        </div>
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
        {snapshot.archive && (
          <div className="job-meta flex-wrap">
            <span>
              내보낸 파일 · {bytes(snapshot.archive.size_bytes)} ·{" "}
              {dateTime(snapshot.archive.exported_at)}
            </span>
            <CopyText value={snapshot.archive.path} />
          </div>
        )}
        {snapshot.error && <p className="job-error">{snapshot.error}</p>}
      </div>
      <div className="flex shrink-0 flex-wrap items-center gap-2">
        <RestoreSnapshotDialog snapshot={snapshot} />
        <ExportSnapshotDialog snapshot={snapshot} />
        <DeleteSnapshotDialog snapshot={snapshot} />
      </div>
    </div>
  );
}
