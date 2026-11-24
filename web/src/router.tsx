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
        Stackboxes로 돌아가기
      </Link>
    </div>
  ),
  errorComponent: ({ error, reset }) => (
    <ErrorNotice error={error} retry={reset} />
  ),
});
const stackboxes = createRoute({
  getParentRoute: () => root,
  path: "/",
  component: lazyRouteComponent(() => import("./pages/stackboxes"), "StackboxesPage"),
});
const detail = createRoute({
  getParentRoute: () => root,
  path: "/stackboxes/$stackboxId",
  component: lazyRouteComponent(
    () => import("./pages/stackbox-detail"),
    "StackboxDetailRoute",
  ),
});
const snapshots = createRoute({
  getParentRoute: () => root,
  path: "/snapshots",
  component: lazyRouteComponent(
    () => import("./pages/snapshots"),
    "SnapshotsPage",
  ),
});
const jobs = createRoute({
  getParentRoute: () => root,
  path: "/jobs",
  component: lazyRouteComponent(() => import("./pages/jobs"), "JobsPage"),
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
  routeTree: root.addChildren([stackboxes, detail, snapshots, jobs, remoteHosts]),
  defaultPreload: "intent",
  scrollRestoration: true,
});
declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}
