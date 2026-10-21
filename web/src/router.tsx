import {
  createRootRoute,
  createRoute,
  createRouter,
  lazyRouteComponent,
  Link,
} from "@tanstack/react-router";
import { Shell } from "./components/shell";
import { ErrorNotice } from "./components/ui";

const root = createRootRoute({
  component: Shell,
  notFoundComponent: () => (
    <div className="empty-state">
      <h1>페이지를 찾을 수 없습니다</h1>
      <Link to="/" className="btn btn-primary">
        Farms로 돌아가기
      </Link>
    </div>
  ),
  errorComponent: ({ error, reset }) => (
    <ErrorNotice error={error} retry={reset} />
  ),
});
const farms = createRoute({
  getParentRoute: () => root,
  path: "/",
  component: lazyRouteComponent(() => import("./pages/farms"), "FarmsPage"),
});
const detail = createRoute({
  getParentRoute: () => root,
  path: "/farms/$farmId",
  component: lazyRouteComponent(
    () => import("./pages/farm-detail"),
    "FarmDetailRoute",
  ),
});
const jobs = createRoute({
  getParentRoute: () => root,
  path: "/jobs",
  component: lazyRouteComponent(() => import("./pages/jobs"), "JobsPage"),
});
const host = createRoute({
  getParentRoute: () => root,
  path: "/host",
  component: lazyRouteComponent(() => import("./pages/host"), "HostPage"),
});
const remoteHosts = createRoute({
  getParentRoute: () => root,
  path: "/hosts",
  component: lazyRouteComponent(
    () => import("./pages/remote-hosts"),
    "RemoteHostsPage",
  ),
});
export const router = createRouter({
  routeTree: root.addChildren([farms, detail, jobs, host, remoteHosts]),
  defaultPreload: "intent",
  scrollRestoration: true,
});
declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}
