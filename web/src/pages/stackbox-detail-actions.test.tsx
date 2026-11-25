import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  createMemoryHistory,
  createRootRoute,
  createRoute,
  createRouter,
  RouterProvider,
} from "@tanstack/react-router";
import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, test, vi } from "vitest";
import type { Job, Stackbox } from "../api/client";
import { StackboxDetailPage } from "./stackbox-detail";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

const plan = {
  components: ["keystone", "glance", "horizon"],
  memory_mib: 3584,
  disk_mib: 86016,
  warnings: [],
  nodes: [{ role: "all-in-one", cpus: 2, disk_mib: 20480, memory_mib: 3584 }],
};

function makeStackbox(overrides: Partial<Stackbox>): Stackbox {
  return {
    id: "workspace",
    status: "ready",
    created_at: 0,
    updated_at: 0,
    host_id: "remote-1",
    agent_sha256: "old-agent-digest",
    plan,
    nodes: [
      {
        name: "workspace-all-in-one",
        role: "all-in-one",
        memory_mib: 3584,
        status: "running",
      },
    ],
    ...overrides,
  } as Stackbox;
}

const succeededDeploy: Job = {
  id: "deploy-1",
  stackbox_id: "workspace",
  kind: "deploy",
  status: "succeeded",
  created_at: 0,
  updated_at: 0,
  events: [],
} as Job;

/** A host whose current agent digest differs from the one the stackbox was created with. */
const remoteHost = {
  id: "remote-1",
  kind: "ssh",
  agent: { artifact: { sha256: "new-agent-digest" } },
};

function renderPage(stackbox: Stackbox, posts: string[] = []) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string, init?: RequestInit) => {
      if (init?.method === "POST") {
        posts.push(path);
        return Response.json({ ...succeededDeploy, id: "deploy-2", status: "queued" });
      }
      if (path === "/api/v1/stackboxes/workspace") return Response.json(stackbox);
      if (path === "/api/v1/jobs") return Response.json([succeededDeploy]);
      if (path === "/api/v1/hosts") return Response.json([remoteHost]);
      return Response.json([]);
    }),
  );
  const root = createRootRoute();
  const index = createRoute({
    getParentRoute: () => root,
    path: "/",
    component: () => null,
  });
  const detail = createRoute({
    getParentRoute: () => root,
    path: "/stackboxes/$stackboxId",
    component: () => <StackboxDetailPage id="workspace" />,
  });
  const router = createRouter({
    routeTree: root.addChildren([index, detail]),
    history: createMemoryHistory({ initialEntries: ["/stackboxes/workspace"] }),
  });
  render(
    <QueryClientProvider client={client}>
      <RouterProvider router={router} />
    </QueryClientProvider>,
  );
  return client;
}

test("a remote agent digest that differs from the host's shows no stale-agent banner", async () => {
  renderPage(makeStackbox({ status: "ready" }));
  // The page has loaded once the meta line names the remote host (its text spans several nodes).
  await waitFor(() => expect(document.body.textContent).toContain("원격 · remote-1"));
  expect(document.body.textContent).not.toContain("예전 Stackbox 실행기에 고정되어 있습니다");
});

test("Ready hides the primary redeploy button and puts 다시 배포 in the ··· menu, which redeploys", async () => {
  const posts: string[] = [];
  renderPage(makeStackbox({ status: "ready" }), posts);
  const user = userEvent.setup();
  const trigger = await screen.findByRole("button", { name: "더 많은 작업" });
  const detailActions = document.querySelector(".detail-actions") as HTMLElement;
  // The header has no primary redeploy button; the only copy sits in the menu.
  expect(within(detailActions).queryByRole("button", { name: "다시 배포" })).toBeNull();

  await user.click(trigger);
  const menu = document.getElementById("stackbox-actions-menu") as HTMLElement;
  expect(within(menu).getByRole("button", { name: "다시 배포" })).toBeTruthy();
  await user.click(within(menu).getByRole("button", { name: "다시 배포" }));
  await user.click(await screen.findByRole("button", { name: "배포 시작" }));
  await waitFor(() =>
    expect(posts).toContain("/api/v1/stackboxes/workspace/deploy"),
  );
});

test("a non-Ready stackbox keeps the primary button and does not repeat it in the menu", async () => {
  renderPage(makeStackbox({ status: "failed" }));
  const user = userEvent.setup();
  const trigger = await screen.findByRole("button", { name: "더 많은 작업" });
  const primary = document.querySelector(
    ".detail-actions .btn-primary",
  ) as HTMLButtonElement;
  expect(primary).toBeTruthy();
  expect(primary.textContent).toContain("OpenStack 배포");

  await user.click(trigger);
  const menu = document.getElementById("stackbox-actions-menu") as HTMLElement;
  expect(within(menu).queryByRole("button", { name: "다시 배포" })).toBeNull();
});
