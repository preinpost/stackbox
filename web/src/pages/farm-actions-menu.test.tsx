import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  createMemoryHistory,
  createRootRoute,
  createRoute,
  createRouter,
  RouterProvider,
} from "@tanstack/react-router";
import { cleanup, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, test, vi } from "vitest";
import type { Farm } from "../api/client";
import { FarmDetailPage } from "./farm-detail";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

const farm: Farm = {
  id: "workspace",
  status: "created",
  created_at: 0,
  updated_at: 0,
  plan: {
    components: ["keystone", "glance", "horizon"],
    memory_mib: 3584,
    disk_mib: 86016,
    warnings: [],
    nodes: [{ role: "all-in-one", cpus: 2, disk_mib: 20480, memory_mib: 3584 }],
  },
  nodes: [
    {
      name: "workspace-all-in-one",
      role: "all-in-one",
      memory_mib: 3584,
      status: "running",
    },
  ],
};

/** The page renders router links, so it needs a router with the routes it points at. */
function renderPage() {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string) => {
      if (path === "/api/v1/farms/workspace") return Response.json(farm);
      if (path === "/api/v1/jobs")
        return Response.json([
          // A finished deploy, so the menu's actions are available.
          {
            id: "deploy-1",
            farm_id: "workspace",
            kind: "deploy",
            status: "succeeded",
            created_at: 0,
            updated_at: 0,
            events: [],
          },
        ]);
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
    path: "/farms/$farmId",
    component: () => <FarmDetailPage id="workspace" />,
  });
  const router = createRouter({
    routeTree: root.addChildren([index, detail]),
    history: createMemoryHistory({ initialEntries: ["/farms/workspace"] }),
  });
  render(
    <QueryClientProvider client={client}>
      <RouterProvider router={router} />
    </QueryClientProvider>,
  );
  return client;
}

test("the overflow menu is linked to its trigger and Escape returns focus to it", async () => {
  const client = renderPage();
  const user = userEvent.setup();
  const trigger = await screen.findByRole("button", { name: "더 많은 작업" });
  expect(trigger.getAttribute("aria-controls")).toBe("farm-actions-menu");
  expect(trigger.getAttribute("aria-expanded")).toBe("false");

  await user.click(trigger);
  const menu = document.getElementById("farm-actions-menu") as HTMLElement;
  expect(menu.hidden).toBe(false);
  expect(trigger.getAttribute("aria-expanded")).toBe("true");

  // Focus lands on a menu item, which the closing menu would otherwise drop on <body>.
  const items = within(menu).getAllByRole("button") as HTMLButtonElement[];
  const item = items.find((button) => !button.disabled);
  expect(item).toBeTruthy();
  item!.focus();
  expect(document.activeElement).toBe(item);

  await user.keyboard("{Escape}");
  expect(menu.hidden).toBe(true);
  expect(trigger.getAttribute("aria-expanded")).toBe("false");
  expect(document.activeElement).toBe(trigger);
  client.clear();
});
