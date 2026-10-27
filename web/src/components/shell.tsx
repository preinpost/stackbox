import { Link, Outlet, useRouterState } from "@tanstack/react-router";
import { useQuery } from "@tanstack/react-query";
import {
  Activity,
  ArrowUpRight,
  Boxes,
  ExternalLink,
  LayoutGrid,
  Menu,
  PanelLeftClose,
  Server,
  Snowflake,
  Terminal,
} from "lucide-react";
import { Fragment, useEffect, useState } from "react";
import { hostQuery, jobsQuery, hostJobsQuery, activeJob } from "../api/client";
import { memory } from "../lib";

type Crumb = {
  label: string;
  to?: "/" | "/hosts" | "/jobs" | "/snapshots";
};

// The last crumb is the current page and stays plain text.
function breadcrumb(pathname: string): Crumb[] {
  if (pathname.startsWith("/hosts")) return [{ label: "Hosts" }];
  if (pathname === "/host")
    return [{ label: "Hosts", to: "/hosts" }, { label: "로컬" }];
  if (pathname.startsWith("/jobs")) return [{ label: "Jobs" }];
  if (pathname.startsWith("/snapshots")) return [{ label: "Snapshots" }];
  const farm = pathname.match(/^\/farms\/([^/]+)/);
  if (farm)
    return [
      { label: "Farms", to: "/" },
      { label: decodeURIComponent(farm[1]) },
    ];
  return [{ label: "Farms" }];
}

export function Shell() {
  const [menuOpen, setMenuOpen] = useState(false);
  useEffect(() => {
    if (!menuOpen) return;
    const close = (event: KeyboardEvent) => {
      if (event.key === "Escape") setMenuOpen(false);
    };
    window.addEventListener("keydown", close);
    return () => window.removeEventListener("keydown", close);
  }, [menuOpen]);
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  const host = useQuery(hostQuery());
  const jobs = useQuery(jobsQuery());
  const hostJobs = useQuery(hostJobsQuery());
  const running =
    (jobs.data?.filter(activeJob).length ?? 0) +
    (hostJobs.data?.filter((j) => ["queued", "running"].includes(j.status))
      .length ?? 0);
  const hostReady =
    !host.isError &&
    host.data?.msb_available &&
    host.data.hypervisor_available &&
    host.data.golden_image_available;
  const navigation = [
    // Hosts come first: a farm is always created on one of them.
    { to: "/hosts" as const, label: "Hosts", icon: Server },
    { to: "/" as const, label: "Farms", icon: LayoutGrid },
    { to: "/snapshots" as const, label: "Snapshots", icon: Snowflake },
    { to: "/jobs" as const, label: "Jobs", icon: Activity },
  ];
  const sectionActive = (to: string) =>
    (to === "/" && pathname.startsWith("/farms/")) ||
    (to === "/hosts" && pathname === "/host");
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
            <Boxes size={19} strokeWidth={1.8} />
          </span>
          <span className="brand-name">
            farm<span className="brand-period">.</span>
          </span>
          <span className="version-tag">LOCAL</span>
        </Link>
        <div className="workspace">
          <span className="workspace-avatar">
            <Terminal size={15} />
            <span className="online-dot" />
          </span>
          <div className="workspace-text">
            <strong>Local workspace</strong>
            <span>OpenStack VMs</span>
          </div>
        </div>
        <p className="nav-label">Workspace</p>
        <nav aria-label="주 메뉴">
          {navigation.map(({ to, label, icon: Icon }) => (
            <Link
              key={to}
              to={to}
              activeOptions={{ exact: to === "/" }}
              className={`nav-item ${sectionActive(to) ? "nav-active" : ""}`}
              activeProps={{ className: "nav-item nav-active" }}
              onClick={() => setMenuOpen(false)}
            >
              <Icon size={17} />
              <span>{label}</span>
              {label === "Jobs" && running > 0 && (
                <span className="nav-count">{running}</span>
              )}
            </Link>
          ))}
        </nav>
        <div className="sidebar-bottom">
          <div className="host-mini">
            <div className="host-mini-head">
              <span
                className={`status-dot ${hostReady ? "text-emerald-500" : "text-amber-500"}`}
              />
              <strong>
                {host.isError
                  ? "연결 확인 필요"
                  : hostReady
                    ? "Host connected"
                    : host.isPending
                      ? "Host 연결 중"
                      : "Host 설정 확인"}
              </strong>
            </div>
            {host.data ? (
              <ul className="host-mini-specs">
                <li>{host.data.arch}</li>
                <li>{host.data.hypervisor.toUpperCase()}</li>
                <li>{memory(host.data.memory_total_mib)}</li>
              </ul>
            ) : (
              <p>farmd 연결 상태를 확인합니다</p>
            )}
            <Link to="/hosts" onClick={() => setMenuOpen(false)}>
              <span>호스트 상태 보기</span>
              <ArrowUpRight size={13} />
            </Link>
          </div>
          <a
            className="documentation-link"
            href="/api/v1/openapi.json"
            target="_blank"
            rel="noreferrer"
          >
            <ExternalLink size={15} />
            <span>API specification</span>
            <ArrowUpRight size={13} />
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
          <div className="flex items-center gap-3">
            <button
              className="icon-button mobile-menu"
              aria-expanded={menuOpen}
              aria-controls="workspace-navigation"
              aria-label={menuOpen ? "메뉴 닫기" : "메뉴 열기"}
              onClick={() => setMenuOpen(!menuOpen)}
            >
              {menuOpen ? <PanelLeftClose size={20} /> : <Menu size={20} />}
            </button>
            <Server size={15} />
            <nav aria-label="현재 위치" className="breadcrumb">
              <Link to="/">Workspace</Link>
              {breadcrumb(pathname).map(({ label, to }, i) => (
                <Fragment key={i}>
                  <span className="text-stone-300">/</span>
                  {to ? (
                    <Link to={to}>{label}</Link>
                  ) : (
                    <strong aria-current="page">{label}</strong>
                  )}
                </Fragment>
              ))}
            </nav>
          </div>
        </header>
        <main id="main-content" className="page-container">
          <Outlet />
        </main>
        <footer className="main-footer">
          <span>farm · OpenStack VM workspace</span>
          <span>호스트에서 직접 실행되는 환경</span>
        </footer>
      </div>
    </div>
  );
}
