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
          { id: "minio", name: "stackbox", prefix: "snapshots/", region: "auto" },
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
        return Response.json({ state_dir: "/tmp/stackbox", arch: "x86_64" });
      if (path.endsWith("/buckets"))
        return Response.json([
          { id: "s3", name: "stackbox-snapshots", prefix: "", region: "us-east-1" },
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

test("lets RestoreSnapshotDialog choose any compatible ready host and updates external host", async () => {
  const { RestoreSnapshotDialog } = await import("./snapshot-actions");
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  let createdBody: unknown;
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string, options?: RequestInit) => {
      if (path.endsWith("/host"))
        return Response.json({ state_dir: "/tmp/stackbox", arch: "aarch64", build_id: "b1" });
      if (path.endsWith("/stackboxes") && options?.method === "POST") {
        createdBody = JSON.parse(String(options.body));
        return Response.json({ id: "job-1", stackbox_id: "restored-on-02", status: "queued" });
      }
      if (path.endsWith("/stackboxes")) return Response.json([]);
      if (path.endsWith("/hosts"))
        return Response.json([
          {
            id: "dev-01",
            kind: "ssh",
            status: "ready",
            address: "10.0.0.21",
            port: 22,
            username: "dev",
            fingerprint: "SHA256:aaa",
            connected: false,
            inspection: { arch: "x86_64", os: "linux", mtty_available: true },
            agent: {
              artifact: {
                sha256: "abc",
                arch: "x86_64",
                protocol: 1,
                supports_single_vm: true,
                build_id: "b1",
              },
            },
            capacity: { memory_budget_mib: 32768, memory_reserved_mib: 0 },
          },
          {
            id: "dev-02",
            kind: "ssh",
            status: "ready",
            address: "10.0.0.21",
            port: 22,
            username: "dev",
            fingerprint: "SHA256:bbb",
            connected: false,
            inspection: { arch: "x86_64", os: "linux", mtty_available: true },
            agent: {
              artifact: {
                sha256: "def",
                arch: "x86_64",
                protocol: 1,
                supports_single_vm: true,
                build_id: "b1",
              },
            },
            capacity: { memory_budget_mib: 32768, memory_reserved_mib: 0 },
          },
          {
            id: "local",
            kind: "local",
            status: "ready",
            address: "127.0.0.1",
            port: 0,
            username: "",
            fingerprint: "",
            connected: false,
            inspection: { arch: "aarch64", os: "macos", mtty_available: false },
            capacity: { memory_budget_mib: 32768, memory_reserved_mib: 0 },
          },
        ]);
      if (options?.method === "POST" && path.endsWith("/stackboxes")) {
        createdBody = JSON.parse(String(options.body));
        return Response.json({ id: "job-1", stackbox_id: "restored-on-02", status: "queued" });
      }
      return Response.json([]);
    }),
  );
  const snapshot = {
    id: "dev-stackbox-001-clean",
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
    source_exposure: { host: "10.0.0.21", bind: "0.0.0.0", port_base: 15000 },
    nodes: [
      { node: "control", reference: "ref-ctl", size_bytes: 100 },
      { node: "compute-1", reference: "ref-cmp", size_bytes: 100 },
    ],
    archive: {
      path: "s3://stackbox-snapshots/dev-stackbox-001-clean.msb",
      size_bytes: 9000,
      exported_at: 10,
      bucket: "s3",
      key: "dev-stackbox-001-clean.msb",
      sha256: "00",
    },
    created_at: 1,
    updated_at: 2,
  };
  const user = userEvent.setup();
  render(
    <QueryClientProvider client={client}>
      <RestoreSnapshotDialog snapshot={snapshot} />
    </QueryClientProvider>,
  );
  await user.click(await screen.findByRole("button", { name: "Stackbox 만들기" }));
  const hostSelect = await screen.findByRole("combobox", { name: "실행 호스트" });
  expect(hostSelect.textContent).toContain("dev-01");
  await user.click(hostSelect);
  const option02 = await screen.findByRole("option", { name: /dev-02/ });
  await user.click(option02);
  expect(
    await screen.findByText(/버킷\(s3\)에서 내려받은 뒤 복원합니다/),
  ).toBeTruthy();
  const extInput = screen.getByLabelText(/외부 접속 호스트/) as HTMLInputElement;
  expect(extInput.value).toBe("10.0.0.21");
  await user.type(screen.getByPlaceholderText("dev-openstack"), "restored-on-02");
  const submit = screen.getAllByRole("button", { name: "Stackbox 만들기" }).at(-1)!;
  await user.click(submit);
  await vi.waitFor(() =>
    expect(createdBody).toEqual({
      id: "restored-on-02",
      host_id: "dev-02",
      snapshot_id: "dev-stackbox-001-clean",
      external_host: "10.0.0.21",
    }),
  );
});

test("CreateFromBucketDialog shows only usable images with metadata matching registered hosts", async () => {
  const { CreateFromBucketDialog } = await import("./snapshot-actions");
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  let createdBody: unknown;
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string, options?: RequestInit) => {
      if (path.endsWith("/host"))
        return Response.json({ state_dir: "/tmp/stackbox", arch: "aarch64", build_id: "b1" });
      if (path.endsWith("/stackboxes") && options?.method === "POST") {
        createdBody = JSON.parse(String(options.body));
        return Response.json({ id: "job-2", stackbox_id: "from-s3", status: "queued" });
      }
      if (path.endsWith("/stackboxes")) return Response.json([]);
      if (path.endsWith("/buckets"))
        return Response.json([
          { id: "s3", name: "stackbox-snapshots", prefix: "", region: "us-east-1" },
        ]);
      if (path.endsWith("/hosts"))
        return Response.json([
          {
            id: "dev-01",
            kind: "ssh",
            status: "ready",
            address: "10.0.0.21",
            port: 22,
            username: "dev",
            fingerprint: "SHA256:aaa",
            connected: false,
            inspection: { arch: "x86_64", os: "linux", mtty_available: true },
            agent: {
              artifact: {
                sha256: "abc",
                arch: "x86_64",
                protocol: 1,
                supports_single_vm: true,
                build_id: "b1",
              },
            },
            capacity: { memory_budget_mib: 32768, memory_reserved_mib: 0 },
          },
        ]);
      if (path.endsWith("/buckets/s3/objects"))
        return Response.json([
          {
            key: "dev-stackbox-001-clean.msb",
            size_bytes: 9_000_000_000,
            metadata: {
              schema: 1,
              key: "dev-stackbox-001-clean.msb",
              size_bytes: 9_000_000_000,
              sha256: "ab",
              snapshot_id: "dev-stackbox-001-clean",
              release: "2026.2",
              role: null,
              components: ["keystone", "nova"],
              computes: 1,
              arch: "x86_64",
              deployed: true,
              created_at: 1,
              exported_at: 2,
            },
          },
          {
            // Compute template: excluded
            key: "cmp.msb",
            size_bytes: 2048,
            metadata: {
              schema: 1,
              key: "cmp.msb",
              size_bytes: 2048,
              sha256: "cd",
              snapshot_id: "cmp",
              release: "2026.2",
              role: "compute",
              components: ["keystone", "nova"],
              computes: 0,
              arch: "x86_64",
              deployed: true,
              created_at: 1,
              exported_at: 2,
            },
          },
          {
            // Different arch: excluded
            key: "arm.msb",
            size_bytes: 2048,
            metadata: {
              schema: 1,
              key: "arm.msb",
              size_bytes: 2048,
              sha256: "ef",
              snapshot_id: "arm",
              release: "2026.2",
              role: null,
              components: ["keystone", "nova"],
              computes: 1,
              arch: "aarch64",
              deployed: true,
              created_at: 1,
              exported_at: 2,
            },
          },
          {
            // No metadata: excluded
            key: "legacy.msb",
            size_bytes: 1024,
            metadata: null,
          },
        ]);
      return Response.json([]);
    }),
  );
  const user = userEvent.setup();
  render(
    <QueryClientProvider client={client}>
      <CreateFromBucketDialog />
    </QueryClientProvider>,
  );
  const trigger = await screen.findByRole("button", { name: /버킷에서 Stackbox 만들기/ });
  await vi.waitFor(() => expect((trigger as HTMLButtonElement).disabled).toBe(false));
  await user.click(trigger);

  expect(await screen.findByText("dev-stackbox-001-clean.msb")).toBeTruthy();
  expect(screen.queryByText("cmp.msb")).toBeNull();
  expect(screen.queryByText("arm.msb")).toBeNull();
  expect(screen.queryByText("legacy.msb")).toBeNull();

  await user.type(screen.getByPlaceholderText("dev-openstack"), "from-s3");
  const submit = screen.getByRole("button", { name: "Stackbox 만들기" });
  await user.click(submit);
  await vi.waitFor(() =>
    expect(createdBody).toEqual({
      id: "from-s3",
      host_id: "dev-01",
      bucket: "s3",
      key: "dev-stackbox-001-clean.msb",
      external_host: "10.0.0.21",
    }),
  );
});
