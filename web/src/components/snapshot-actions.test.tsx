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

/** Hosts with a stackbox-kernel PCI inspection: one ready device, one taken, one not ready. */
const pciRestoreHosts = (ids: string[] = ["pci-01"]) =>
  ids.map((id) => ({
    id,
    kind: "ssh",
    status: "ready",
    address: "10.0.0.21",
    port: 22,
    username: "dev",
    fingerprint: "SHA256:ccc",
    connected: false,
    inspection: {
      arch: "x86_64",
      os: "linux",
      mtty_available: false,
      msb_pci_capable: true,
      pci_passthrough_available: true,
      pci_devices: [
        {
          bdf: "0000:04:00.3",
          vendor_id: "1b21",
          product_id: "2142",
          class: "0c0330",
          iommu_group: 17,
          driver: "vfio-pci",
          group_exclusive: true,
          vfio_ready: true,
        },
        {
          bdf: "0000:02:00.0",
          vendor_id: "10de",
          product_id: "2684",
          class: "030000",
          iommu_group: 9,
          driver: "vfio-pci",
          group_exclusive: true,
          vfio_ready: true,
        },
        {
          bdf: "0000:03:00.0",
          vendor_id: "8086",
          product_id: "1521",
          class: "020000",
          iommu_group: 5,
          driver: "vfio-pci",
          group_exclusive: true,
          vfio_ready: false,
        },
      ],
    },
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
  }));

/** A stackbox on pci-01 that already passes 0000:02:00.0 through. */
const pciBusyStackbox = {
  id: "busy-stackbox",
  status: "ready",
  host_id: "pci-01",
  created_at: 0,
  updated_at: 0,
  plan: {
    components: ["keystone"],
    memory_mib: 1024,
    disk_mib: 1024,
    warnings: [],
    nodes: [],
    pci_devices: ["0000:02:00.0"],
  },
};

const pciSnapshot = (computes: number, control = false) =>
  ({
    id: "pci-snap",
    status: "ready",
    arch: "x86_64",
    host_id: "pci-01",
    template: control ? "control" : undefined,
    plan: {
      components: ["keystone", "nova"],
      computes,
      nodes: control
        ? [{ role: "control", name: "control" }]
        : [
            { role: "control", name: "control" },
            { role: "compute", name: "compute-1", index: 1 },
          ],
      memory_mib: 12544,
      disk_mib: 100000,
      warnings: [],
    },
    deployed: true,
    source_exposure: { host: "10.0.0.21", bind: "0.0.0.0", port_base: 15000 },
    nodes: [],
    created_at: 1,
    updated_at: 2,
  }) as never;

const pciRestoreFetch = (
  hosts: unknown[],
  created: { body?: unknown },
) =>
  vi.fn(async (path: string, options?: RequestInit) => {
    if (path.endsWith("/host"))
      return Response.json({ state_dir: "/tmp/stackbox", arch: "x86_64", build_id: "b1" });
    if (path.endsWith("/stackboxes") && options?.method === "POST") {
      created.body = JSON.parse(String(options.body));
      return Response.json({ id: "job-pci", stackbox_id: "pci-restored", status: "queued" });
    }
    if (path.endsWith("/stackboxes")) return Response.json([pciBusyStackbox]);
    if (path.endsWith("/hosts")) return Response.json(hosts);
    return Response.json([]);
  });

const pciCheckbox = async (bdf: string) =>
  (await screen.findByText(bdf)).closest("label")!;
const isDisabledCheckbox = (box: Element) =>
  box.getAttribute("aria-disabled") === "true" ||
  box.hasAttribute("disabled") ||
  (box as HTMLElement).dataset.disabled === "true";

test("RestoreSnapshotDialog offers PCI devices for a multi-node snapshot and sends the picked one", async () => {
  const { RestoreSnapshotDialog } = await import("./snapshot-actions");
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  const created: { body?: unknown } = {};
  vi.stubGlobal("fetch", pciRestoreFetch(pciRestoreHosts(), created));
  const user = userEvent.setup();
  render(
    <QueryClientProvider client={client}>
      <RestoreSnapshotDialog snapshot={pciSnapshot(1)} />
    </QueryClientProvider>,
  );
  await user.click(await screen.findByRole("button", { name: "Stackbox 만들기" }));
  expect(await screen.findByText("PCI 패스스루 (compute-1)")).toBeTruthy();

  const ready = await pciCheckbox("0000:04:00.3");
  expect(ready.textContent).toContain("vfio-pci 준비됨");
  const checkReady = ready.querySelector("[role=checkbox]")!;
  expect(isDisabledCheckbox(checkReady)).toBe(false);

  const taken = await pciCheckbox("0000:02:00.0");
  expect(taken.textContent).toContain("사용 중(busy-stackbox)");
  expect(isDisabledCheckbox(taken.querySelector("[role=checkbox]")!)).toBe(true);

  const notReady = await pciCheckbox("0000:03:00.0");
  expect(notReady.textContent).toContain("준비 안 됨");
  expect(isDisabledCheckbox(notReady.querySelector("[role=checkbox]")!)).toBe(true);

  await user.click(checkReady);
  await user.type(screen.getByPlaceholderText("dev-openstack"), "pci-restored");
  await user.click(screen.getAllByRole("button", { name: "Stackbox 만들기" }).at(-1)!);
  await vi.waitFor(() =>
    expect(created.body).toEqual({
      id: "pci-restored",
      host_id: "pci-01",
      snapshot_id: "pci-snap",
      external_host: "10.0.0.21",
      pci_devices: ["0000:04:00.3"],
    }),
  );
});

test("RestoreSnapshotDialog hides PCI devices for a control-only snapshot", async () => {
  const { RestoreSnapshotDialog } = await import("./snapshot-actions");
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  vi.stubGlobal("fetch", pciRestoreFetch(pciRestoreHosts(), {}));
  const user = userEvent.setup();
  render(
    <QueryClientProvider client={client}>
      <RestoreSnapshotDialog snapshot={pciSnapshot(0, true)} />
    </QueryClientProvider>,
  );
  await user.click(await screen.findByRole("button", { name: "Stackbox 만들기" }));
  // The host list has loaded once its name shows in the host select.
  await vi.waitFor(() =>
    expect(
      screen.getByRole("combobox", { name: "실행 호스트" }).textContent,
    ).toContain("pci-01"),
  );
  expect(screen.queryByText("PCI 패스스루 (compute-1)")).toBeNull();
  expect(screen.queryByText("0000:04:00.3")).toBeNull();
});

test("RestoreSnapshotDialog drops the picked PCI device when the restore host changes", async () => {
  const { RestoreSnapshotDialog } = await import("./snapshot-actions");
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  const created: { body?: unknown } = {};
  vi.stubGlobal(
    "fetch",
    pciRestoreFetch(pciRestoreHosts(["pci-01", "pci-02"]), created),
  );
  const user = userEvent.setup();
  render(
    <QueryClientProvider client={client}>
      <RestoreSnapshotDialog snapshot={pciSnapshot(1)} />
    </QueryClientProvider>,
  );
  await user.click(await screen.findByRole("button", { name: "Stackbox 만들기" }));
  const ready = await pciCheckbox("0000:04:00.3");
  await user.click(ready.querySelector("[role=checkbox]")!);
  expect(ready.querySelector("[role=checkbox]")!.getAttribute("aria-checked")).toBe(
    "true",
  );

  await user.click(screen.getByRole("combobox", { name: "실행 호스트" }));
  await user.click(await screen.findByRole("option", { name: /pci-02/ }));
  await vi.waitFor(() =>
    expect(
      screen.getByRole("combobox", { name: "실행 호스트" }).textContent,
    ).toContain("pci-02"),
  );
  const again = await pciCheckbox("0000:04:00.3");
  expect(again.querySelector("[role=checkbox]")!.getAttribute("aria-checked")).toBe(
    "false",
  );
  await user.type(screen.getByPlaceholderText("dev-openstack"), "pci-moved");
  await user.click(screen.getAllByRole("button", { name: "Stackbox 만들기" }).at(-1)!);
  await vi.waitFor(() => expect(created.body).toBeDefined());
  expect(created.body).toMatchObject({ host_id: "pci-02" });
  expect(created.body).not.toHaveProperty("pci_devices");
});
