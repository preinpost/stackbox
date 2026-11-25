import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  createMemoryHistory,
  createRootRoute,
  createRoute,
  createRouter,
  RouterProvider,
} from "@tanstack/react-router";
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, expect, test, vi } from "vitest";
import { Shell } from "./shell";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

test("the sidebar brand shows the logo and name without a LOCAL badge", async () => {
  vi.stubGlobal(
    "fetch",
    vi.fn(async () => Response.json([])),
  );
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  const root = createRootRoute({ component: Shell });
  const index = createRoute({
    getParentRoute: () => root,
    path: "/",
    component: () => null,
  });
  const router = createRouter({
    routeTree: root.addChildren([index]),
    history: createMemoryHistory({ initialEntries: ["/"] }),
  });
  const { container } = render(
    <QueryClientProvider client={client}>
      <RouterProvider router={router} />
    </QueryClientProvider>,
  );
  const brand = await screen.findByRole("link", { name: /stackbox/ });
  expect(brand.querySelector(".brand-mark")).toBeTruthy();
  expect(brand.querySelector(".brand-name")?.textContent).toBe("stackbox.");
  expect(container.querySelector(".version-tag")).toBeNull();
  expect(container.textContent).not.toContain("LOCAL");
  client.clear();
});
