import { Dialog } from "@base-ui/react/dialog";
import { Tabs } from "@base-ui/react/tabs";
import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import {
  flexRender,
  getCoreRowModel,
  getExpandedRowModel,
  getPaginationRowModel,
  useReactTable,
  type ColumnDef,
  type ExpandedState,
} from "@tanstack/react-table";
import {
  Activity as ActivityIcon,
  ArrowRight,
  Boxes,
  CheckCheck,
  ChevronDown,
  ChevronLeft,
  ChevronRight,
  CircleAlert,
  Clock3,
  Search,
  Server,
  X,
} from "lucide-react";
import { Fragment, useEffect, useMemo, useState } from "react";
import {
  activeJob,
  ApiError,
  farmsQuery,
  hostJobsQuery,
  jobsQuery,
  logsQuery,
} from "../api/client";
import {
  Button,
  EmptyState,
  ErrorNotice,
  Loading,
  PageHeading,
  SelectInput,
  StatusBadge,
} from "../components/ui";
import { dateTime, jobKindLabels, phases } from "../lib";
import {
  activityLabel,
  activityStep,
  elapsed,
  failureSummary,
  historyRows,
  subject,
  startedAt,
  type Activity,
  type ActivityRow,
  type HistoryFilters,
} from "./jobs-model";

export function JobsMonitor() {
  const jobs = useQuery(jobsQuery());
  const hostJobs = useQuery(hostJobsQuery());
  const [scope, setScope] = useState("farm");
  const farmActivity = useMemo<Activity[]>(
    () => (jobs.data ?? []).map((job) => ({ scope: "farm", job })),
    [jobs.data],
  );
  const hostActivity = useMemo<Activity[]>(
    () => (hostJobs.data ?? []).map((job) => ({ scope: "host", job })),
    [hostJobs.data],
  );
  return (
    <>
      <PageHeading
        eyebrow="ACTIVITY"
        title="Jobs"
        description="진행 중인 작업을 확인하고, 대상별 실행 이력을 찾아보세요."
      />
      <Tabs.Root
        value={scope}
        onValueChange={(value) => setScope(String(value))}
      >
        <Tabs.List className="tabs-list jobs-scope" aria-label="작업 범위">
          <Tabs.Tab value="farm" className="tab">
            <Boxes size={16} />
            Farm 작업{" "}
            <span className="count-pill">
              {jobs.data?.filter(activeJob).length ?? 0} 진행 중
            </span>
          </Tabs.Tab>
          <Tabs.Tab value="host" className="tab">
            <Server size={16} />
            호스트 작업{" "}
            <span className="count-pill">
              {hostJobs.data?.filter(activeJob).length ?? 0} 진행 중
            </span>
          </Tabs.Tab>
        </Tabs.List>
        <Tabs.Panel value="farm" className="tab-panel">
          <JobWorkspace
            scope="farm"
            activities={farmActivity}
            pending={jobs.isPending}
            error={jobs.error}
            retry={() => void jobs.refetch()}
          />
        </Tabs.Panel>
        <Tabs.Panel value="host" className="tab-panel">
          <JobWorkspace
            scope="host"
            activities={hostActivity}
            pending={hostJobs.isPending}
            error={hostJobs.error}
            retry={() => void hostJobs.refetch()}
          />
        </Tabs.Panel>
      </Tabs.Root>
    </>
  );
}

const defaultFilters: HistoryFilters = {
  search: "",
  status: "all",
  kind: "all",
  days: "all",
};
function JobWorkspace({
  scope,
  activities,
  pending,
  error,
  retry,
}: {
  scope: "farm" | "host";
  activities: Activity[];
  pending: boolean;
  error: unknown;
  retry: () => void;
}) {
  const [filters, setFilters] = useState(defaultFilters);
  const [grouped, setGrouped] = useState(true);
  const [selected, setSelected] = useState<Activity | null>(null);
  const [now, setNow] = useState(() => Date.now() / 1000);
  const running = activities
    .filter((a) => activeJob(a.job))
    .sort((a, b) => b.job.created_at - a.job.created_at);
  const hasRunning = running.length > 0;
  useEffect(() => {
    if (!hasRunning) return;
    const tick = setInterval(() => setNow(Date.now() / 1000), 1000);
    return () => clearInterval(tick);
  }, [hasRunning]);
  // Keep an open detail panel current as query polling advances the job.
  const current =
    selected &&
    (activities.find((a) => a.job.id === selected.job.id) ?? selected);
  const data = useMemo(
    () => historyRows(activities, filters, grouped, Date.now() / 1000),
    [activities, filters, grouped],
  );
  const historyCount = data.reduce(
    (sum, row) => sum + row.previous.length + 1,
    0,
  );
  const setFilter = (key: keyof HistoryFilters, value: string) =>
    setFilters((previous) => ({ ...previous, [key]: value }));
  if (pending) return <Loading label="작업을 불러오는 중" />;
  if (error) return <ErrorNotice error={error} retry={retry} />;
  return (
    <>
      <section className="jobs-live" aria-label="진행 중 작업">
        <div className="jobs-section-heading">
          <h2>
            <ActivityIcon size={16} />
            진행 중 <span className="count-pill">{running.length}</span>
          </h2>
          <span className="muted">
            {hasRunning ? "자동 업데이트" : "모든 작업이 종료되었습니다"}
          </span>
        </div>
        {hasRunning ? (
          <div className="jobs-live-grid">
            {running.map((a) => (
              <button
                key={a.job.id}
                className="jobs-live-card"
                onClick={() => setSelected(a)}
                aria-label={`${subject(a)} 진행 중 작업 상세`}
              >
                <div className="jobs-live-title">
                  <strong>{subject(a)}</strong>
                  <StatusBadge status={a.job.status} />
                </div>
                <span>
                  {activityLabel(a)}
                  <span className="muted">
                    {" "}
                    ·{" "}
                    {a.job.status === "queued"
                      ? "실행 대기"
                      : `${elapsed(a, now)} 경과`}
                  </span>
                </span>
                <div className="jobs-current-step">
                  <Clock3 size={14} />
                  <span>{activityStep(a)}</span>
                  <ArrowRight size={15} />
                </div>
                {a.scope === "farm" &&
                  a.job.kind === "deploy" &&
                  a.job.phase && (
                    <div
                      className="jobs-phase-track"
                      aria-label={`배포 ${phases.findIndex((p) => p.id === a.job.phase) + 1} / ${phases.length}단계`}
                    >
                      {phases.map((p, index) => (
                        <i
                          key={p.id}
                          className={
                            index <=
                            phases.findIndex((p) => p.id === a.job.phase)
                              ? "reached"
                              : ""
                          }
                        />
                      ))}
                    </div>
                  )}
              </button>
            ))}
          </div>
        ) : (
          <div className="jobs-idle">
            <CheckCheck size={18} />
            <span>
              진행 중인 {scope === "farm" ? "Farm" : "호스트"} 작업이 없습니다.
            </span>
          </div>
        )}
      </section>
      <section className="panel jobs-history" aria-label="완료된 작업 이력">
        <div className="jobs-history-heading">
          <div>
            <h2>
              최근 이력{" "}
              <span className="count-pill">
                {activities.filter((a) => !activeJob(a.job)).length}
              </span>
            </h2>
            <p>완료·실패·중단된 작업입니다. 이전 시도는 펼쳐서 확인하세요.</p>
          </div>
          <label className="jobs-group-toggle">
            <input
              type="checkbox"
              checked={grouped}
              onChange={(e) => setGrouped(e.target.checked)}
            />
            {scope === "farm" ? "Farm" : "호스트"}별로 묶기
          </label>
        </div>
        <div className="jobs-filters">
          <div className="search-input">
            <Search size={16} />
            <input
              aria-label="작업 검색"
              placeholder={`${scope === "farm" ? "Farm" : "호스트"} 이름 또는 작업 ID 검색`}
              value={filters.search}
              onChange={(e) => setFilter("search", e.target.value)}
            />
          </div>
          <SelectInput
            label="작업 상태"
            value={filters.status}
            onChange={(v) => setFilter("status", v)}
            items={[
              { value: "all", label: "모든 상태" },
              { value: "succeeded", label: "완료" },
              { value: "failed", label: "실패" },
              { value: "interrupted", label: "중단됨" },
            ]}
          />
          <SelectInput
            label="작업 종류"
            value={filters.kind}
            onChange={(v) => setFilter("kind", v)}
            items={
              scope === "farm"
                ? [
                    { value: "all", label: "모든 작업" },
                    ...Object.entries(jobKindLabels).map(([value, label]) => ({
                      value,
                      label,
                    })),
                  ]
                : [
                    { value: "all", label: "모든 작업" },
                    { value: "check", label: "SSH 점검" },
                    { value: "prepare", label: "환경 준비" },
                    { value: "install_agent", label: "Farm 실행기 설치" },
                  ]
            }
          />
          <SelectInput
            label="작업 기간"
            value={filters.days}
            onChange={(v) => setFilter("days", v)}
            items={[
              { value: "all", label: "전체 기간" },
              { value: "1", label: "최근 24시간" },
              { value: "7", label: "최근 7일" },
              { value: "30", label: "최근 30일" },
            ]}
          />
        </div>
        {data.length ? (
          <HistoryTable
            key={`${JSON.stringify(filters)}:${grouped}`}
            data={data}
            grouped={grouped}
            now={now}
            scope={scope}
            count={historyCount}
            select={setSelected}
          />
        ) : (
          <EmptyState
            icon={<Search size={24} />}
            title={
              activities.some((a) => !activeJob(a.job))
                ? "조건에 맞는 작업이 없습니다"
                : "아직 완료된 작업이 없습니다"
            }
            description="작업이 끝나면 이곳에 기록됩니다. 검색어나 필터를 변경해 이력을 찾아보세요."
            action={
              Object.entries(filters).some(
                ([key, value]) =>
                  value !== defaultFilters[key as keyof HistoryFilters],
              ) ? (
                <Button onClick={() => setFilters(defaultFilters)}>
                  필터 초기화
                </Button>
              ) : undefined
            }
          />
        )}
      </section>
      <Dialog.Root
        open={Boolean(current)}
        onOpenChange={(open) => {
          if (!open) setSelected(null);
        }}
      >
        {current && <ActivityDetail activity={current} now={now} />}
      </Dialog.Root>
    </>
  );
}

function HistoryTable({
  data,
  grouped,
  now,
  scope,
  count,
  select,
}: {
  data: ActivityRow[];
  grouped: boolean;
  now: number;
  scope: "farm" | "host";
  count: number;
  select: (a: Activity) => void;
}) {
  const [expanded, setExpanded] = useState<ExpandedState>({});
  const columns = useMemo<ColumnDef<ActivityRow>[]>(
    () => [
      {
        id: "target",
        header: scope === "farm" ? "Farm" : "호스트",
        cell: ({ row }) => (
          <div className={`jobs-target ${row.depth ? "jobs-previous" : ""}`}>
            <button
              className="jobs-target-name"
              onClick={() => select(row.original.activity)}
            >
              {subject(row.original.activity)}
            </button>
            {row.getCanExpand() && (
              <button
                className="jobs-expand"
                aria-expanded={row.getIsExpanded()}
                aria-label={`${subject(row.original.activity)} 이전 작업 ${row.subRows.length}개`}
                onClick={row.getToggleExpandedHandler()}
              >
                <ChevronDown
                  size={13}
                  className={row.getIsExpanded() ? "rotate-180" : ""}
                />
                이전 {row.subRows.length}개
              </button>
            )}
            {row.depth > 0 && <small>이전 작업</small>}
          </div>
        ),
      },
      {
        id: "kind",
        header: "작업",
        cell: ({ row }) => activityLabel(row.original.activity),
      },
      {
        id: "status",
        header: "상태",
        cell: ({ row }) => (
          <StatusBadge status={row.original.activity.job.status} />
        ),
      },
      {
        id: "phase",
        header: "단계 / 결과",
        cell: ({ row }) => (
          <span
            className={
              failureSummary(row.original.activity)
                ? "jobs-failure-summary"
                : "muted"
            }
          >
            {failureSummary(row.original.activity) ??
              activityStep(row.original.activity)}
          </span>
        ),
      },
      {
        id: "created",
        header: "시작 시각",
        cell: ({ row }) => (
          <time
            dateTime={new Date(
              startedAt(row.original.activity) * 1000,
            ).toISOString()}
          >
            {dateTime(startedAt(row.original.activity))}
          </time>
        ),
      },
      {
        id: "duration",
        header: "소요 시간",
        cell: ({ row }) => (
          <span className="jobs-duration">
            {elapsed(row.original.activity, now)}
          </span>
        ),
      },
      {
        id: "detail",
        header: "",
        cell: ({ row }) => (
          <button
            className="icon-button"
            aria-label={`${subject(row.original.activity)} ${activityLabel(row.original.activity)} 상세 ${row.original.activity.job.id}`}
            onClick={() => select(row.original.activity)}
          >
            <ArrowRight size={16} />
          </button>
        ),
      },
    ],
    [scope, now, select],
  );
  const table = useReactTable({
    data,
    columns,
    state: { expanded },
    onExpandedChange: setExpanded,
    getRowId: (row) => row.activity.job.id,
    getSubRows: (row) => row.previous,
    getCoreRowModel: getCoreRowModel(),
    getExpandedRowModel: getExpandedRowModel(),
    getPaginationRowModel: getPaginationRowModel(),
    paginateExpandedRows: false,
    autoResetPageIndex: false,
    initialState: { pagination: { pageSize: 20 } },
  });
  useEffect(() => {
    const lastPage = Math.max(0, table.getPageCount() - 1);
    if (table.getState().pagination.pageIndex > lastPage)
      table.setPageIndex(lastPage);
  }, [table, data]);
  return (
    <>
      <div
        className="table-scroll"
        tabIndex={0}
        role="region"
        aria-label="작업 이력 표"
      >
        <table className="data-table jobs-table">
          <thead>
            {table.getHeaderGroups().map((group) => (
              <tr key={group.id}>
                {group.headers.map((header) => (
                  <th key={header.id}>
                    {flexRender(
                      header.column.columnDef.header,
                      header.getContext(),
                    )}
                  </th>
                ))}
              </tr>
            ))}
          </thead>
          <tbody>
            {table.getRowModel().rows.map((row) => (
              <tr key={row.id} className={row.depth ? "jobs-previous-row" : ""}>
                {row.getVisibleCells().map((cell) => (
                  <td key={cell.id}>
                    {flexRender(cell.column.columnDef.cell, cell.getContext())}
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <div className="table-footer">
        <span>
          {count}개 작업
          {grouped
            ? ` · ${data.length}개 ${scope === "farm" ? "Farm" : "호스트"}`
            : ""}{" "}
          · {table.getState().pagination.pageIndex + 1} /{" "}
          {Math.max(1, table.getPageCount())} 페이지
        </span>
        <div className="flex gap-1">
          <Button
            variant="ghost"
            aria-label="이전 페이지"
            disabled={!table.getCanPreviousPage()}
            onClick={() => table.previousPage()}
          >
            <ChevronLeft size={16} />
          </Button>
          <Button
            variant="ghost"
            aria-label="다음 페이지"
            disabled={!table.getCanNextPage()}
            onClick={() => table.nextPage()}
          >
            <ChevronRight size={16} />
          </Button>
        </div>
      </div>
    </>
  );
}

function ActivityDetail({
  activity: a,
  now,
}: {
  activity: Activity;
  now: number;
}) {
  const farms = useQuery({ ...farmsQuery(), enabled: a.scope === "farm" });
  const farm =
    a.scope === "farm"
      ? farms.data?.find((f) => f.id === a.job.farm_id)
      : undefined;
  return (
    <Dialog.Portal>
      <Dialog.Backdrop className="modal-backdrop" />
      <Dialog.Viewport className="job-drawer-viewport">
        <Dialog.Popup className="job-drawer">
          <div className="job-drawer-heading">
            <div>
              <p className="eyebrow">
                {a.scope === "farm" ? "FARM JOB" : "HOST JOB"}
              </p>
              <Dialog.Title>{subject(a)}</Dialog.Title>
              <Dialog.Description>
                {activityLabel(a)} · 작업 상세
              </Dialog.Description>
            </div>
            <Dialog.Close className="icon-button" aria-label="상세 닫기">
              <X size={19} />
            </Dialog.Close>
          </div>
          <div className="job-drawer-body">
            <StatusBadge status={a.job.status} />
            <dl className="job-detail-facts">
              <dt>작업 ID</dt>
              <dd className="font-mono">{a.job.id}</dd>
              <dt>생성 시각</dt>
              <dd>{dateTime(a.job.created_at)}</dd>
              {a.scope === "farm" && a.job.started_at != null && (
                <>
                  <dt>시작 시각</dt>
                  <dd>{dateTime(a.job.started_at)}</dd>
                </>
              )}
              <dt>종료 시각</dt>
              <dd>
                {a.job.finished_at != null ? dateTime(a.job.finished_at) : "—"}
              </dd>
              <dt>소요 시간</dt>
              <dd>{elapsed(a, now)}</dd>
              <dt>단계</dt>
              <dd>{activityStep(a)}</dd>
            </dl>
            {a.job.error && (
              <section className="job-detail-error">
                <h3>
                  <CircleAlert size={15} />
                  {failureSummary(a) ?? "오류 상세"}
                </h3>
                <pre>{a.job.error}</pre>
              </section>
            )}
            {a.scope === "host" ? (
              <section className="job-detail-log">
                <h3>작업 이벤트</h3>
                {a.job.events.length ? (
                  <ol className="job-events">
                    {a.job.events.map((event, index) => (
                      <li key={index}>
                        <time>{dateTime(event.at)}</time>
                        <p>{event.message}</p>
                      </li>
                    ))}
                  </ol>
                ) : (
                  <p className="muted">아직 기록된 이벤트가 없습니다.</p>
                )}
              </section>
            ) : (
              <FarmJobLog activity={a} />
            )}
          </div>
          <div className="job-drawer-footer">
            {a.scope === "host" ? (
              <Link to="/hosts" className="btn btn-default">
                Hosts로 이동
                <ArrowRight size={14} />
              </Link>
            ) : farm && farm.status !== "deleted" ? (
              <Link
                to="/farms/$farmId"
                params={{ farmId: a.job.farm_id }}
                className="btn btn-default"
              >
                Farm 상세로 이동
                <ArrowRight size={14} />
              </Link>
            ) : (
              <span className="muted">
                {farms.isPending
                  ? "Farm 상태 확인 중…"
                  : farms.error
                    ? "Farm 상태를 확인하지 못했습니다."
                    : "삭제된 Farm의 작업 기록입니다."}
              </span>
            )}
            <Dialog.Close render={<Button />}>닫기</Dialog.Close>
          </div>
        </Dialog.Popup>
      </Dialog.Viewport>
    </Dialog.Portal>
  );
}
function FarmJobLog({
  activity: a,
}: {
  activity: Extract<Activity, { scope: "farm" }>;
}) {
  const logs = useQuery({
    ...logsQuery(a.job.farm_id),
    refetchInterval: activeJob(a.job) ? 1500 : false,
  });
  const entries = logs.data?.filter((log) => log.job_id === a.job.id) ?? [];
  return (
    <section className="job-detail-log">
      <h3>작업 로그</h3>
      <p className="field-hint">이 작업에 해당하는 최근 보관 로그입니다.</p>
      {logs.isPending ? (
        <Loading label="로그를 불러오는 중" />
      ) : logs.error instanceof ApiError && logs.error.status === 404 ? (
        <p className="jobs-log-empty">
          Farm이 삭제되어 작업 로그를 조회할 수 없습니다. 작업 결과와 오류
          상세는 위에 보관됩니다.
        </p>
      ) : logs.error ? (
        <ErrorNotice error={logs.error} retry={() => void logs.refetch()} />
      ) : !entries.length ? (
        <p className="jobs-log-empty">보관 범위에 이 작업의 로그가 없습니다.</p>
      ) : (
        <pre className="job-log-output">
          {entries.map((entry) => (
            <Fragment key={entry.id}>
              <span className="muted">
                {new Date(entry.timestamp * 1000).toLocaleTimeString("en-GB")}
                {entry.phase ? ` [${entry.phase}]` : ""}
              </span>{" "}
              {entry.message.trimEnd()}
              {"\n"}
            </Fragment>
          ))}
        </pre>
      )}
    </section>
  );
}
