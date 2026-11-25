import { Link, Outlet, useRouterState } from "@tanstack/react-router";
import { useQuery } from "@tanstack/react-query";
import {
  Activity,
  ArrowRight,
  ArrowUpRight,
  Boxes,
  ExternalLink,
  LayoutGrid,
  Menu,
  Moon,
  PanelLeftClose,
  Server,
  Snowflake,
  Sun,
  Terminal,
} from "lucide-react";
import { Fragment, useEffect, useState } from "react";
import {
  hostQuery,
  hostsQuery,
  jobsQuery,
  hostJobsQuery,
  activeJob,
} from "../api/client";
import { hostReady, memory } from "../lib";
import {
  applyTheme,
  preferredTheme,
  setTheme as storeTheme,
  watchSystemTheme,
  type Theme,
} from "../theme";

type Crumb = {
  label: string;
  to?: "/" | "/hosts" | "/jobs" | "/snapshots";
};

// The last crumb is the current page and stays plain text.
function breadcrumb(pathname: string): Crumb[] {
  if (pathname.startsWith("/hosts")) return [{ label: "Hosts" }];
  if (pathname.startsWith("/jobs")) return [{ label: "Jobs" }];
  if (pathname.startsWith("/snapshots")) return [{ label: "Snapshots" }];
  const stackbox = pathname.match(/^\/stackboxes\/([^/]+)/);
  if (stackbox)
    return [
      { label: "Stackboxes", to: "/" },
      { label: decodeURIComponent(stackbox[1]) },
    ];
  return [{ label: "Stackboxes" }];
}

export function Shell() {
  const [menuOpen, setMenuOpen] = useState(false);
  const [theme, setCurrentTheme] = useState<Theme>(() => preferredTheme());
  useEffect(() => {
    applyTheme(theme);
  }, [theme]);
  // Until the operator picks a theme, the page follows the OS as it changes.
  useEffect(() => watchSystemTheme(setCurrentTheme), []);
  useEffect(() => {
    if (!menuOpen) return;
    const close = (event: KeyboardEvent) => {
      if (event.key === "Escape") setMenuOpen(false);
    };
    window.addEventListener("keydown", close);
    return () => window.removeEventListener("keydown", close);
  }, [menuOpen]);
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  const coordinator = useQuery(hostQuery());
  const hosts = useQuery(hostsQuery());
  const jobs = useQuery(jobsQuery());
  const hostJobs = useQuery(hostJobsQuery());
  const running =
    (jobs.data?.filter(activeJob).length ?? 0) +
    (hostJobs.data?.filter((j) => ["queued", "running"].includes(j.status))
      .length ?? 0);
  const readyHosts =
    hosts.data?.filter((h) => hostReady(h, coordinator.data?.build_id)) ?? [];
  const capacities =
    hosts.data?.flatMap((h) => (h.capacity ? [h.capacity] : [])) ?? [];
  const reserved = capacities.reduce(
    (sum, c) => sum + c.memory_reserved_mib,
    0,
  );
  const budget = capacities.reduce((sum, c) => sum + c.memory_budget_mib, 0);
  const reservation = budget ? Math.min(100, (reserved / budget) * 100) : 0;
  const navigation = [
    // Hosts come first: a stackbox is always created on one of them.
    { to: "/hosts" as const, label: "Hosts", icon: Server },
    { to: "/" as const, label: "Stackboxes", icon: LayoutGrid },
    { to: "/snapshots" as const, label: "Snapshots", icon: Snowflake },
    { to: "/jobs" as const, label: "Jobs", icon: Activity },
  ];
  const sectionActive = (to: string) =>
    to === "/" && pathname.startsWith("/stackboxes/");
  const navActive = (to: string) =>
    to === "/"
      ? pathname === "/" || pathname.startsWith("/stackboxes/")
      : pathname.startsWith(to);
  const dark = theme === "dark";
  return (
    <div className="app-layout">
      {menuOpen && (
        <button
          className="sidebar-shade"
          aria-label="메뉴 닫기"
          onClick={() => setMenuOpen(false)}
        />
      )}
      <aside
        id="workspace-navigation"
        className={`sidebar ${menuOpen ? "is-open" : ""}`}
      >
        <Link to="/" className="brand" onClick={() => setMenuOpen(false)}>
          <span className="brand-mark">
            <Boxes size={18} strokeWidth={1.8} />
          </span>
          <span className="brand-name">
            stackbox<span className="brand-period">.</span>
          </span>
        </Link>
        <div className="workspace">
          <span className="workspace-avatar">
            <Terminal size={16} />
            <span className="online-dot" />
          </span>
          <div className="workspace-text">
            <strong>Local workspace</strong>
            <span>OpenStack VMs</span>
          </div>
        </div>
        <div className="sidebar-nav">
          <p className="nav-label">Workspace</p>
          <nav className="nav" aria-label="주 메뉴">
            {navigation.map(({ to, label, icon: Icon }) => (
              <Link
                key={to}
                to={to}
                activeOptions={{ exact: to === "/" }}
                className={`nav-item ${sectionActive(to) ? "nav-active" : ""}`}
                activeProps={{ className: "nav-item nav-active" }}
                aria-current={navActive(to) ? "page" : undefined}
                onClick={() => setMenuOpen(false)}
              >
                <Icon size={16} />
                <span>{label}</span>
                {label === "Jobs" && running > 0 && (
                  <span className="nav-count live">{running}</span>
                )}
              </Link>
            ))}
          </nav>
        </div>
        <div className="sidebar-bottom">
          <div className="host-mini">
            <div className="host-mini-head">
              <span
                className={`status-dot ${readyHosts.length ? "is-ready" : "is-pending"}`}
              />
              <strong>
                {hosts.isError
                  ? "연결 확인 필요"
                  : hosts.isPending
                    ? "호스트 확인 중"
                    : !hosts.data?.length
                      ? "호스트 등록 필요"
                      : `호스트 ${readyHosts.length} / ${hosts.data.length} 준비됨`}
              </strong>
              {hosts.data?.length ? (
                <span className="host-dots" aria-hidden="true">
                  {hosts.data.slice(0, 5).map((h) => (
                    <i
                      key={h.id}
                      className={`host-dot ${
                        readyHosts.some((ready) => ready.id === h.id)
                          ? "on"
                          : "setup"
                      }`}
                    />
                  ))}
                </span>
              ) : null}
            </div>
            {hosts.data?.length ? (
              <>
                <div
                  className="mini-meter"
                  role="img"
                  aria-label={`호스트 예약 메모리 ${Math.round(reservation)}%`}
                >
                  <i style={{ width: `${reservation}%` }} />
                </div>
                <div className="host-mini-foot">
                  <span>
                    예약 {memory(reserved)} / {memory(budget)}
                  </span>
                  <Link to="/hosts" onClick={() => setMenuOpen(false)}>
                    <span>호스트 관리</span>
                    <ArrowRight size={13} />
                  </Link>
                </div>
                <ul className="host-mini-specs">
                  {hosts.data.slice(0, 3).map((h) => (
                    <li key={h.id}>{h.id}</li>
                  ))}
                </ul>
              </>
            ) : (
              <p>
                {hosts.isError
                  ? "stackboxd 연결 상태를 확인합니다"
                  : "로컬 호스트나 원격 서버를 등록하세요"}
              </p>
            )}
            {!hosts.data?.length && (
              <Link to="/hosts" onClick={() => setMenuOpen(false)}>
                <span>호스트 관리</span>
                <ArrowRight size={13} />
              </Link>
            )}
          </div>
          <a
            className="documentation-link"
            href="/api/v1/openapi.json"
            target="_blank"
            rel="noreferrer"
          >
            <ExternalLink size={15} />
            <span>API specification</span>
            <ArrowUpRight size={14} />
          </a>
          <div className="sidebar-footer">
            <span className="operator-avatar">L</span>
            <div className="sidebar-footer-text">
              <strong>Local operator</strong>
              <span>Host workspace</span>
            </div>
          </div>
        </div>
      </aside>
      <div className="main-layout">
        <header className="topbar">
          <button
            className="icon-button mobile-menu"
            aria-expanded={menuOpen}
            aria-controls="workspace-navigation"
            aria-label={menuOpen ? "메뉴 닫기" : "메뉴 열기"}
            onClick={() => setMenuOpen(!menuOpen)}
          >
            {menuOpen ? <PanelLeftClose size={18} /> : <Menu size={18} />}
          </button>
          <nav aria-label="현재 위치" className="breadcrumb">
            <Link to="/" className="crumb-workspace">
              Workspace
            </Link>
            {breadcrumb(pathname).map(({ label, to }, i) => (
              <Fragment key={i}>
                <span className="crumb-sep" aria-hidden="true">
                  /
                </span>
                {to ? (
                  <Link to={to}>{label}</Link>
                ) : (
                  <strong aria-current="page">{label}</strong>
                )}
              </Fragment>
            ))}
          </nav>
          <span className="topbar-spacer" />
          {coordinator.data?.build_id && (
            <span
              className="build-chip"
              title={`stackboxd build ${coordinator.data.build_id}`}
            >
              <span className="dot" aria-hidden="true" />
              stackboxd {coordinator.data.build_id.slice(0, 7)}
            </span>
          )}
          <button
            className="icon-button line"
            type="button"
            aria-pressed={dark}
            aria-label="다크 모드"
            title="다크 모드"
            onClick={() => {
              const next: Theme = dark ? "light" : "dark";
              storeTheme(next);
              setCurrentTheme(next);
            }}
          >
            {dark ? <Sun size={16} /> : <Moon size={16} />}
          </button>
          <span
            className="operator-avatar"
            title="Local operator"
            aria-hidden="true"
          >
            L
          </span>
        </header>
        <main id="main-content" className="page-container">
          <Outlet />
        </main>
        <footer className="main-footer">
          <span>stackbox · OpenStack VM workspace</span>
          <span>호스트에서 직접 실행되는 환경</span>
        </footer>
      </div>
    </div>
  );
}
