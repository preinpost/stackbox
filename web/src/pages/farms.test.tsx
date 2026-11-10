import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, expect, test, vi } from "vitest";
import type { Farm } from "../api/client";
import { FarmsPage } from "./farms";

vi.mock("@tanstack/react-router", () => ({
  Link: ({ children }: { children: React.ReactNode }) => <a>{children}</a>,
}));
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

const plan = (release?: string): Farm["plan"] => ({
  components: ["keystone"],
  ...(release ? { release } : {}),
  memory_mib: 3072,
  disk_mib: 20480,
  warnings: [],
  nodes: [],
});
const farms: Farm[] = [
  {
    id: "current",
    status: "ready",
    created_at: 0,
    updated_at: 0,
    plan: plan("2024.1"),
    openstack: {
      release: "2024.1",
      series: "2024.1",
      eol: false,
      supported: true,
    },
    nodes: [],
  },
  {
    id: "zed-farm",
    status: "ready",
    created_at: 0,
    updated_at: 0,
    plan: plan("2022.2"),
    openstack: {
      release: "2022.2",
      series: "zed",
      eol: true,
      supported: false,
    },
    nodes: [],
  },
  {
    id: "no-images",
    status: "created",
    created_at: 0,
    updated_at: 0,
    plan: plan("2023.1"),
    openstack: {
      release: "2023.1",
      series: "2023.1",
      eol: false,
      supported: false,
    },
    nodes: [],
  },
  // A farm recorded before plans kept their release.
  {
    id: "unrecorded",
    status: "created",
    created_at: 0,
    updated_at: 0,
    plan: plan(),
    nodes: [],
  },
];

test("shows each farm's OpenStack release in its own column, flagged outside support", async () => {
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string) => {
      if (path.endsWith("/farms")) return Response.json(farms);
      if (path.endsWith("/hosts")) return Response.json([]);
      if (path.endsWith("/jobs")) return Response.json([]);
      throw new Error(`unexpected request ${path}`);
    }),
  );
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  render(
    <QueryClientProvider client={client}>
      <FarmsPage />
    </QueryClientProvider>,
  );
  await screen.findByRole("table");
  const row = (id: string) => {
    const found = screen
      .getAllByRole("row")
      .find((tr) => tr.textContent?.includes(id));
    if (!found) throw new Error(`no row for ${id}`);
    return found;
  };
  expect(screen.getByRole("columnheader", { name: "OpenStack" })).toBeTruthy();
  // The year names are the series names, so nothing is repeated.
  expect(row("current").textContent).toContain("OpenStack 2024.1");
  expect(row("current").querySelector(".release-flag")).toBeNull();
  // zed names the images and the release notes of 2022.2, which is end of life.
  expect(row("zed-farm").textContent).toContain("OpenStack 2022.2 (zed)");
  expect(row("zed-farm").querySelector(".release-flag")?.textContent).toBe(
    "EOL",
  );
  // 2023.1 has no images at all, so it is simply outside the supported range.
  expect(row("no-images").querySelector(".release-flag")?.textContent).toBe(
    "지원 범위 밖",
  );
  // A farm that kept no release says so rather than guessing one.
  expect(row("unrecorded").textContent).toContain("릴리스 미기록");
  client.clear();
});
