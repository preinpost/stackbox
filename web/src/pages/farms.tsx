import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import {
  flexRender,
  getCoreRowModel,
  getFilteredRowModel,
  getPaginationRowModel,
  getSortedRowModel,
  useReactTable,
  type ColumnDef,
  type SortingState,
} from "@tanstack/react-table";
import {
  Activity,
  ArrowDown,
  ArrowRight,
  ArrowUpDown,
  Boxes,
  Check,
  ChevronLeft,
  ChevronRight,
  Cpu,
  Layers,
  Monitor,
  Network,
  Search,
  Server,
} from "lucide-react";
import { useMemo, useState } from "react";
import {
  activeJob,
  farmsQuery,
  hostsQuery,
  jobsQuery,
  type Farm,
} from "../api/client";
import { CreateFarmDialog } from "../components/farm-actions";
import { FarmRelease, farmReleaseLabel } from "../components/farm-release";
import {
  Button,
  EmptyState,
  ErrorNotice,
  Loading,
  PageHeading,
  SelectInput,
  StatusBadge,
} from "../components/ui";
import {
  componentLabel,
  dateTime,
  farmDeployed,
  farmMemory,
  farmNodeCount,
  farmState,
  jobKindLabels,
  memory,
} from "../lib";

/** The mock shows four services and folds the rest into a count. */
const tagLimit = 4;

export function FarmsPage() {
  const farms = useQuery(farmsQuery());
  const hosts = useQuery(hostsQuery());
  const jobs = useQuery(jobsQuery());
  const [search, setSearch] = useState("");
  const [status, setStatus] = useState("all");
  const [sorting, setSorting] = useState<SortingState>([
    { id: "created_at", desc: true },
  ]);
  const data = useMemo(
    () =>
      (farms.data ?? []).filter((f) => status === "all" || f.status === status),
    [farms.data, status],
  );
  // A snapshot job leaves its farm ready, so each row reads the job it is running.
  const activeJobs = useMemo(
    () => new Map((jobs.data ?? []).map((job) => [job.id, job])),
    [jobs.data],
  );
  // A local farm runs on this machine, which the list names instead of the host ID.
  const localHosts = useMemo(
    () =>
      new Set(
        (hosts.data ?? []).filter((h) => h.kind === "local").map((h) => h.id),
      ),
    [hosts.data],
  );
  const columns = useMemo<ColumnDef<Farm>[]>(
    () => [
      {
        accessorKey: "id",
        header: "Farm",
        cell: ({ row }) => (
          <Link
            to="/farms/$farmId"
            params={{ farmId: row.original.id }}
            className="farm-name-cell"
          >
            <span className="farm-avatar">
              <Boxes size={16} strokeWidth={1.7} />
            </span>
            <span>
              <strong>{row.original.id}</strong>
              <small>{row.original.owner || "담당자 미지정"}</small>
            </span>
          </Link>
        ),
      },
      {
        id: "openstack",
        header: "OpenStack",
        accessorFn: (farm) =>
          farm.openstack?.release ?? farm.plan?.release ?? "",
        cell: ({ row }) => (
          <span className="release-cell">
            {farmReleaseLabel(row.original) ? (
              <FarmRelease farm={row.original} />
            ) : (
              "릴리스 미기록"
            )}
          </span>
        ),
      },
      {
        id: "host",
        header: "호스트",
        accessorFn: (farm) => farm.host_id ?? "—",
        cell: ({ row }) => {
          const host = row.original.host_id;
          const local = host ? localHosts.has(host) : false;
          return (
            <span className="hostname">
              {local ? (
                <Monitor size={14} strokeWidth={1.9} />
              ) : (
                <Server size={14} strokeWidth={1.9} />
              )}
              {host ? (local ? "로컬" : host) : "—"}
            </span>
          );
        },
      },
      {
        accessorKey: "status",
        header: "상태",
        cell: ({ row }) => {
          const job = activeJobs.get(row.original.active_job ?? "");
          return (
            <>
              <StatusBadge status={farmState(row.original, job)} />
              {job && (
                <span className="cellnote">
                  {jobKindLabels[job.kind]}
                  {job.phase ? ` · ${job.phase}` : ""}
                </span>
              )}
            </>
          );
        },
      },
      {
        id: "resources",
        header: "VM",
        // The column counts running VMs, so the header sorts by that count.
        accessorFn: (farm) =>
          farm.nodes.filter((n) => n.status === "running").length,
        cell: ({ row }) => (
          <span className="vmcount">
            {row.original.nodes.filter((n) => n.status === "running").length}
            <small> / {farmNodeCount(row.original)}</small>
            <small className="cellnote">
              {memory(farmMemory(row.original))}
            </small>
          </span>
        ),
      },
      {
        id: "services",
        header: "서비스",
        enableSorting: false,
        cell: ({ row }) => {
          const components = row.original.plan?.components;
          if (!components?.length)
            return <span className="muted">기존 환경</span>;
          const shown = components.slice(0, tagLimit);
          return (
            <div className="tags">
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
          );
        },
      },
      {
        accessorKey: "created_at",
        header: "생성",
        cell: ({ row }) => (
          <span className="date-cell">{dateTime(row.original.created_at)}</span>
        ),
      },
      {
        id: "action",
        header: "",
        enableSorting: false,
        cell: ({ row }) => (
          <Link
            to="/farms/$farmId"
            params={{ farmId: row.original.id }}
            className="icon-button"
            aria-label={`${row.original.id} 상세 보기`}
          >
            <ChevronRight size={16} />
          </Link>
        ),
      },
    ],
    [activeJobs, localHosts],
  );
  const table = useReactTable({
    data,
    columns,
    state: { sorting, globalFilter: search },
    onSortingChange: setSorting,
    onGlobalFilterChange: setSearch,
    globalFilterFn: (row, _id, value: string) =>
      `${row.original.id} ${row.original.owner ?? ""}`
        .toLowerCase()
        .includes(value.toLowerCase().trim()),
    getCoreRowModel: getCoreRowModel(),
    getFilteredRowModel: getFilteredRowModel(),
    getSortedRowModel: getSortedRowModel(),
    getPaginationRowModel: getPaginationRowModel(),
    initialState: { pagination: { pageSize: 8 } },
  });
  // Every host reserves guest memory against its own budget; this sums them.
  const capacities = (hosts.data ?? []).flatMap((h) =>
    h.capacity ? [h.capacity] : [],
  );
  const reserved = capacities.reduce(
    (sum, c) => sum + c.memory_reserved_mib,
    0,
  );
  const budget = capacities.reduce((sum, c) => sum + c.memory_budget_mib, 0);
  const ratio = budget ? Math.min(100, (reserved / budget) * 100) : 0;
  return (
    <>
      <PageHeading
        eyebrow="YOUR ENVIRONMENTS"
        title="Farms"
        description="OpenStack 환경을 만들고, 배포하고, 한곳에서 관리하세요."
        actions={<CreateFarmDialog />}
      />
      <section className="metrics-grid" aria-label="Farm 요약">
        <Metric
          label="전체 farm"
          icon={<Layers size={16} />}
          value={farms.data?.length}
          detail={
            <>
              <span className="metric-positive">
                {farms.data?.filter((f) => farmDeployed(f)).length ?? "—"} Ready
              </span>
              <span className="metric-separator">·</span>
              {farms.data?.filter((f) =>
                ["failed", "broken"].includes(f.status),
              ).length ?? "—"}{" "}
              점검 필요
            </>
          }
        />
        <Metric
          label="실행 중인 VM"
          icon={<Server size={16} />}
          value={farms.data?.reduce(
            (sum, f) =>
              sum + f.nodes.filter((n) => n.status === "running").length,
            0,
          )}
          detail="farm마다 독립된 가상 머신"
        />
        <div className="metric-card">
          <div className="metric-label">
            호스트 예약 메모리
            <Cpu size={16} />
          </div>
          <div className="metric-value">
            {hosts.data ? memory(reserved) : "—"}
            <small>/ {hosts.data ? memory(budget) : "—"}</small>
          </div>
          <div className="meter">
            <span style={{ width: `${ratio}%` }} />
          </div>
        </div>
        <Metric
          label="진행 중인 작업"
          icon={<Activity size={16} />}
          value={jobs.data?.filter(activeJob).length}
          detail={
            <Link to="/jobs" className="metric-link">
              작업 내역 보기
              <ArrowRight size={14} />
            </Link>
          }
        />
      </section>
      {farms.error && (
        <ErrorNotice
          error={farms.error}
          retry={() => {
            void farms.refetch();
          }}
        />
      )}
      <div className="toolbar">
        <div className="toolbar-right">
          <div className="search-input">
            <Search size={16} />
            <input
              aria-label="Farm 검색"
              placeholder="이름 또는 담당자 검색…"
              value={search}
              onChange={(e) => setSearch(e.target.value)}
            />
          </div>
          <SelectInput
            label="Farm 상태 필터"
            value={status}
            onChange={setStatus}
            items={[
              { value: "all", label: "모든 상태" },
              { value: "ready", label: "Ready" },
              { value: "snapshotting", label: "얼리는 중" },
              { value: "created", label: "VM 준비됨" },
              { value: "creating", label: "생성 중" },
              { value: "deploying", label: "배포 중" },
              { value: "deleting", label: "삭제 중" },
              { value: "failed", label: "실패" },
              { value: "broken", label: "점검 필요" },
            ]}
          />
        </div>
      </div>
      <section className="panel" aria-label="Farm 목록">
        {farms.isPending ? (
          <Loading label="Farm을 불러오는 중" />
        ) : !farms.error && !farms.data?.length ? (
          <EmptyState
            icon={<FarmIllustration />}
            title="첫 번째 farm을 만들어보세요"
            description="control VM과 compute VM으로 이루어진 독립된 OpenStack 환경. 프로필과 설치할 서비스를 선택하세요."
            action={<CreateFarmDialog prominent />}
          />
        ) : farms.data && table.getRowModel().rows.length === 0 ? (
          <EmptyState
            icon={<Search size={24} />}
            title="검색 결과가 없습니다"
            description="다른 이름을 검색하거나 상태 필터를 변경해보세요."
            action={
              <Button
                onClick={() => {
                  setSearch("");
                  setStatus("all");
                }}
              >
                필터 초기화
              </Button>
            }
          />
        ) : (
          farms.data && (
            <>
              <div className="table-scroll">
                <table className="data-table">
                  <thead>
                    {table.getHeaderGroups().map((g) => (
                      <tr key={g.id}>
                        {g.headers.map((h) => (
                          <th key={h.id}>
                            {h.column.getCanSort() ? (
                              <button
                                onClick={h.column.getToggleSortingHandler()}
                              >
                                {flexRender(
                                  h.column.columnDef.header,
                                  h.getContext(),
                                )}
                                {h.column.getIsSorted() ? (
                                  <ArrowDown
                                    size={14}
                                    className={
                                      h.column.getIsSorted() === "asc"
                                        ? "rotate-180"
                                        : ""
                                    }
                                  />
                                ) : (
                                  <ArrowUpDown size={14} />
                                )}
                              </button>
                            ) : (
                              flexRender(
                                h.column.columnDef.header,
                                h.getContext(),
                              )
                            )}
                          </th>
                        ))}
                      </tr>
                    ))}
                  </thead>
                  <tbody>
                    {table.getRowModel().rows.map((row) => (
                      <tr key={row.id}>
                        {row.getVisibleCells().map((cell) => (
                          <td key={cell.id}>
                            {flexRender(
                              cell.column.columnDef.cell,
                              cell.getContext(),
                            )}
                          </td>
                        ))}
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
              <div className="table-footer">
                <span>
                  {table.getFilteredRowModel().rows.length}개 farm ·{" "}
                  {table.getState().pagination.pageIndex + 1} /{" "}
                  {Math.max(1, table.getPageCount())} 페이지
                </span>
                <div className="flex gap-1">
                  <Button
                    variant="ghost"
                    aria-label="이전 페이지"
                    onClick={() => table.previousPage()}
                    disabled={!table.getCanPreviousPage()}
                  >
                    <ChevronLeft size={16} />
                  </Button>
                  <Button
                    variant="ghost"
                    aria-label="다음 페이지"
                    onClick={() => table.nextPage()}
                    disabled={!table.getCanNextPage()}
                  >
                    <ChevronRight size={16} />
                  </Button>
                </div>
              </div>
            </>
          )
        )}
      </section>
      <div className="below-grid">
        <section className="panel host-overview" aria-label="호스트 용량">
          <div className="panel-toolbar">
            <h2 className="flex items-center gap-2">
              <Cpu size={16} />
              호스트 용량
            </h2>
            <Link to="/hosts" className="text-link">
              Hosts
              <ArrowRight size={14} />
            </Link>
          </div>
          <div className="panel-body">
            {hosts.error ? (
              <ErrorNotice error={hosts.error} />
            ) : hosts.isPending ? (
              <Loading />
            ) : !capacities.length ? (
              <p className="capacity-note">
                준비된 호스트가 없습니다. Hosts에서 로컬 호스트나 원격 서버를
                등록하세요.
              </p>
            ) : (
              hosts.data && (
                <>
                  <div className="capacity-top">
                    <span>
                      {memory(reserved)} <small>예약됨</small>
                    </span>
                    <span className="muted">한도 {memory(budget)}</span>
                  </div>
                  <div className="capacity-meter">
                    <span style={{ width: `${ratio}%` }} />
                  </div>
                  <div className="capacity-legend">
                    <span>
                      <i />
                      Farm 예약
                    </span>
                    <span>
                      <i className="ghost" />
                      추가 예약 가능 {memory(Math.max(0, budget - reserved))}
                    </span>
                  </div>
                  <p className="capacity-note">
                    점검한 호스트 {capacities.length}대의 게스트 메모리 예약
                    합계입니다. 현재 호스트 사용량과는 다를 수 있습니다.
                  </p>
                </>
              )
            )}
          </div>
        </section>
        <section className="getting-started" aria-label="작업 흐름">
          <div className="panel-toolbar">
            <h2 className="flex items-center gap-2">
              <Network size={16} />
              작은 환경, 완전한 워크플로
            </h2>
          </div>
          <div className="workflow-steps">
            <span>
              <b>01</b>VM 생성
            </span>
            <ArrowRight size={14} />
            <span>
              <b>02</b>OpenStack 배포
            </span>
            <ArrowRight size={14} />
            <span>
              <b>03</b>API 연결
              <Check size={14} />
            </span>
          </div>
          <p>
            Keystone · Glance · Placement · Horizon · Neutron · Nova 지원 ·
            배포는 한 번에 하나씩 실행됩니다.
          </p>
        </section>
      </div>
    </>
  );
}
function Metric({
  label,
  icon,
  value,
  detail,
}: {
  label: string;
  icon: React.ReactNode;
  value: number | undefined;
  detail: React.ReactNode;
}) {
  return (
    <div className="metric-card">
      <div className="metric-label">
        {label}
        {icon}
      </div>
      <div className="metric-value">
        {value ?? "—"}
        <small>개</small>
      </div>
      <div className="metric-detail">{detail}</div>
    </div>
  );
}
function FarmIllustration() {
  return (
    <div className="farm-illustration" aria-hidden="true">
      <span className="illustration-line horizontal" />
      <span className="illustration-line vertical" />
      <span className="illustration-node side-node left-node">
        <Server size={19} />
      </span>
      <span className="illustration-node main-node">
        <Boxes size={29} strokeWidth={1.5} />
        <i>
          <Check size={13} strokeWidth={3} />
        </i>
      </span>
      <span className="illustration-node side-node right-node">
        <Server size={19} />
      </span>
    </div>
  );
}
