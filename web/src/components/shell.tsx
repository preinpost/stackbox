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
import { useEffect, useState } from "react";
import { hostQuery, jobsQuery, hostJobsQuery, activeJob } from "../api/client";
import { memory } from "../lib";

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
            <Boxes size={23} strokeWidth={1.7} />
          </span>
          <span>
            farm<span className="brand-period">.</span>
          </span>
          <span className="version-tag">LOCAL</span>
        </Link>
        <div className="workspace">
          <span className="workspace-avatar">
            <Terminal size={17} />
          </span>
          <div>
            <strong>Local workspace</strong>
            <span>OpenStack environments</span>
          </div>
          <span className="online-dot" />
        </div>
        <p className="nav-label">WORKSPACE</p>
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
              <Icon size={18} />
              <span>{label}</span>
              {label === "Jobs" && running > 0 && (
                <span className="nav-count">{running}</span>
              )}
            </Link>
          ))}
        </nav>
        <div className="sidebar-bottom">
          <div className="host-mini">
            <div className="flex items-center gap-2">
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
            <p>
              {host.data
                ? `${host.data.arch} · ${host.data.hypervisor.toUpperCase()} · ${memory(host.data.memory_total_mib)}`
                : "farmd 연결 상태를 확인합니다"}
            </p>
            <Link to="/hosts">
              호스트 상태 보기
              <ArrowUpRight size={13} />
            </Link>
          </div>
          <a
            className="documentation-link"
            href="/api/v1/openapi.json"
            target="_blank"
            rel="noreferrer"
          >
            <ExternalLink size={14} />
            API specification
            <ArrowUpRight size={13} />
          </a>
          <div className="sidebar-footer">
            <span className="operator-avatar">L</span>
            <div>
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
            <span>Workspace</span>
            <span className="text-stone-300">/</span>
            <strong>
              {pathname.startsWith("/hosts")
                ? "Hosts"
                : pathname === "/host"
                  ? "Hosts / 로컬"
                  : pathname.startsWith("/jobs")
                    ? "Jobs"
                    : pathname.startsWith("/snapshots")
                      ? "Snapshots"
                      : "Farms"}
            </strong>
          </div>
          <div className="topbar-right">
            <span className="topbar-tag">MICROSANDBOX</span>
            <span className="topbar-divider" />
            <span className="flex items-center gap-2">
              <span
                className={`status-dot ${hostReady ? "text-emerald-500" : "text-amber-500"}`}
              />
              {hostReady
                ? "시스템 정상"
                : host.isPending
                  ? "연결 중"
                  : "호스트 확인"}
            </span>
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
