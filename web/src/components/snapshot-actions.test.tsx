import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, cleanup } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, test, vi } from "vitest";
import { ImportSnapshotDialog } from "./snapshot-actions";

vi.mock("@tanstack/react-router", () => ({
  useNavigate: () => vi.fn(),
  Link: ({ children }: { children: React.ReactNode }) => <a>{children}</a>,
}));
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

test("lists bucket archives by what their metadata files say", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  let submitted: unknown;
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string, options?: RequestInit) => {
      if (path.endsWith("/buckets"))
        return Response.json([
          { id: "minio", name: "farm", prefix: "snapshots/", region: "auto" },
        ]);
      if (path.endsWith("/buckets/minio/objects"))
        return Response.json([
          {
            key: "snapshots/ctl.msb",
            size_bytes: 2048,
            metadata: {
              schema: 1,
              key: "snapshots/ctl.msb",
              size_bytes: 2048,
              sha256: "00",
              snapshot_id: "ctl",
              release: "2025.1",
              role: "control",
              components: ["keystone"],
              arch: "aarch64",
              deployed: true,
              created_at: 1,
              exported_at: 2,
            },
          },
          { key: "snapshots/old.msb", size_bytes: 1024, metadata: null },
        ]);
      if (options?.method === "POST") {
        submitted = JSON.parse(String(options.body));
        return Response.json({ id: "old", status: "importing" });
      }
      return Response.json([]);
    }),
  );
  render(
    <QueryClientProvider client={client}>
      <ImportSnapshotDialog />
    </QueryClientProvider>,
  );
  await userEvent.click(
    await screen.findByRole("button", { name: /가져오기/ }),
  );
  expect(
    await screen.findByText("OpenStack 2025.1 · Control 템플릿"),
  ).toBeTruthy();
  // An archive from before metadata files stays importable.
  const legacy = screen.getByRole("radio", { name: /snapshots\/old\.msb/ });
  expect(legacy.textContent).toContain("메타데이터 없음");
  await userEvent.click(legacy);
  await userEvent.click(screen.getByRole("button", { name: /가져오기 시작/ }));
  await vi.waitFor(() =>
    expect(submitted).toEqual({ bucket: "minio", key: "snapshots/old.msb" }),
  );
});

test("enables export for multi-node snapshots and shows compute count in bucket list", async () => {
  const { ExportSnapshotDialog } = await import("./snapshot-actions");
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  let exportedBody: unknown;
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string, options?: RequestInit) => {
      if (path.endsWith("/host"))
        return Response.json({ state_dir: "/tmp/farm", arch: "x86_64" });
      if (path.endsWith("/buckets"))
        return Response.json([
          { id: "s3", name: "farm-snapshots", prefix: "", region: "us-east-1" },
        ]);
      if (options?.method === "POST" && path.endsWith("/snapshots/mn-snap/export")) {
        exportedBody = JSON.parse(String(options.body));
        return Response.json({ id: "mn-snap", status: "exporting" });
      }
      return Response.json([]);
    }),
  );
  const snapshot = {
    id: "mn-snap",
    status: "ready" as const,
    arch: "x86_64",
    host_id: "dev-01",
    plan: {
      components: ["keystone", "nova"],
      computes: 1,
      nodes: [],
      memory_mib: 12544,
      disk_mib: 100000,
      warnings: [],
    },
    deployed: true,
    source_exposure: { host: "127.0.0.1", bind: "0.0.0.0", port_base: 15000 },
    nodes: [
      { node: "control", reference: "ref-ctl", size_bytes: 100 },
      { node: "compute-1", reference: "ref-cmp", size_bytes: 100 },
    ],
    created_at: 1,
    updated_at: 2,
  };
  render(
    <QueryClientProvider client={client}>
      <ExportSnapshotDialog snapshot={snapshot} />
    </QueryClientProvider>,
  );
  const trigger = await screen.findByRole("button", { name: /내보내기/ });
  expect((trigger as HTMLButtonElement).disabled).toBe(false);
  await userEvent.click(trigger);
  expect(
    await screen.findByText(/2개 VM의 \.msb 아카이브를 하나의 파일로 묶어/),
  ).toBeTruthy();
  await userEvent.click(screen.getByRole("button", { name: /내보내기 시작/ }));
  await vi.waitFor(() =>
    expect(exportedBody).toEqual({ bucket: "s3", key: "mn-snap.msb" }),
  );
});
