import { Dialog } from "@base-ui/react/dialog";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, cleanup, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, test, vi } from "vitest";
import { keys, type Farm, type Plan, type Snapshot } from "../api/client";
import {
  AddComputeDialog,
  CreateFarmForm,
  DeleteFarmDialog,
  DeployFarmDialog,
} from "./farm-actions";
import {
  DeleteSnapshotDialog,
  FreezeFarmDialog,
  RestoreSnapshotDialog,
} from "./snapshot-actions";

const { navigate } = vi.hoisted(() => ({
  navigate: vi.fn().mockResolvedValue(undefined),
}));
vi.mock("@tanstack/react-router", () => ({
  useNavigate: () => navigate,
  Link: ({ children }: { children: React.ReactNode }) => <a>{children}</a>,
}));
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

const releaseCatalog = {
  arch: "aarch64",
  releases: [
    {
      release: "2026.2",
      local_control: 0,
      local_compute: 0,
      bucket_control: 0,
      bucket_compute: 0,
      installable: true,
    },
  ],
  unreachable: [],
};
const releasePreview = (body: {
  id: string;
  host_id?: string;
  components?: string[];
}) => ({
  farm_id: body.id,
  release: "2026.2",
  arch: "aarch64",
  components: body.components ?? [],
  control: {
    node: "control",
    host_id: body.host_id ?? "local",
    source: "clean",
    reason: "installed clean",
  },
  computes: [],
  notes: [],
});

test("defaults to a ready host and sends its ID and public hostname", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  let submitted: unknown;
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string, options?: RequestInit) => {
      if (path.endsWith("/catalog"))
        return Response.json({
          components: [{ id: "keystone" }, { id: "glance" }],
        });
      if (path.endsWith("/plans")) return Response.json(singleVmPlan());
      if (path.endsWith("/host"))
        return Response.json({
          msb_available: false,
          memory_budget_mib: 0,
          memory_reserved_mib: 0,
        });
      if (path.endsWith("/hosts"))
        return Response.json([
          {
            id: "local",
            kind: "local",
            address: "localhost",
            status: "needs_setup",
            capacity: { memory_budget_mib: 8000, memory_reserved_mib: 0 },
          },
          {
            id: "target",
            kind: "ssh",
            address: "10.0.0.21",
            status: "ready",
            agent: { artifact: { supports_single_vm: true } },
            inspection: { checked_at: Date.now() / 1000 },
            capacity: { memory_budget_mib: 8000, memory_reserved_mib: 0 },
          },
        ]);
      if (options?.method === "POST" && !path.includes("/releases/plan")) {
        submitted = JSON.parse(String(options.body));
        return Response.json({
          id: "job",
          farm_id: "remote-test",
          status: "queued",
        });
      }
      if (path.endsWith("/releases")) return Response.json(releaseCatalog);
      if (path.endsWith("/releases/plan"))
        return Response.json(releasePreview(JSON.parse(String(options?.body))));
      return Response.json([]);
    }),
  );
  render(
    <QueryClientProvider client={client}>
      <Dialog.Root open>
        <CreateFarmForm close={vi.fn()} onPending={vi.fn()} />
      </Dialog.Root>
    </QueryClientProvider>,
  );
  const user = userEvent.setup();
  await user.type(screen.getByPlaceholderText("dev-openstack"), "remote-test");
  const select = screen.getByRole("combobox", { name: "실행 호스트" });
  await waitFor(() => expect(select.textContent).toContain("target · 원격"));
  await user.click(select);
  expect(
    (
      await screen.findByRole("option", { name: "local · 로컬 · 준비 필요" })
    ).getAttribute("aria-disabled"),
  ).toBe("true");
  await user.click(screen.getByRole("combobox", { name: "실행 호스트" }));
  await waitFor(() =>
    expect(
      (screen.getByRole("button", { name: "Farm 만들기" }) as HTMLButtonElement)
        .disabled,
    ).toBe(false),
  );
  await user.click(screen.getByRole("button", { name: "Farm 만들기" }));
  await waitFor(() =>
    expect(submitted).toMatchObject({
      id: "remote-test",
      host_id: "target",
      external_host: "10.0.0.21",
    }),
  );
  client.clear();
});

test("opens on the host it was launched from and refuses hosts that are not ready", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  const checked = { checked_at: Date.now() / 1000 };
  const hosts = [
    {
      id: "local",
      kind: "local",
      address: "localhost",
      status: "ready",
      capacity: { memory_budget_mib: 8000, memory_reserved_mib: 0 },
    },
    {
      id: "target",
      kind: "ssh",
      address: "10.0.0.21",
      status: "ready",
      agent: { artifact: { supports_single_vm: true } },
      inspection: checked,
      capacity: { memory_budget_mib: 8000, memory_reserved_mib: 0 },
    },
    {
      id: "old-agent",
      kind: "ssh",
      address: "10.0.0.23",
      status: "ready",
      agent: { artifact: {} },
      inspection: checked,
      capacity: { memory_budget_mib: 8000, memory_reserved_mib: 0 },
    },
    {
      id: "pending",
      kind: "ssh",
      address: "10.0.0.22",
      status: "needs_setup",
      inspection: checked,
      capacity: { memory_budget_mib: 8000, memory_reserved_mib: 0 },
    },
  ];
  let submitted: unknown;
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string, options?: RequestInit) => {
      if (path.endsWith("/catalog"))
        return Response.json({
          components: [{ id: "keystone" }, { id: "glance" }],
        });
      if (path.endsWith("/plans")) return Response.json(singleVmPlan());
      if (path.endsWith("/host"))
        return Response.json({
          msb_available: true,
          hypervisor_available: true,
          golden_image_available: true,
          memory_budget_mib: 8000,
          memory_reserved_mib: 0,
        });
      if (path.endsWith("/hosts")) return Response.json(hosts);
      if (options?.method === "POST" && !path.includes("/releases/plan")) {
        submitted = JSON.parse(String(options.body));
        return Response.json({ id: "job", farm_id: "on-target" });
      }
      if (path.endsWith("/releases")) return Response.json(releaseCatalog);
      if (path.endsWith("/releases/plan"))
        return Response.json(releasePreview(JSON.parse(String(options?.body))));
      return Response.json([]);
    }),
  );
  // The Hosts page has already loaded the host list when it opens the dialog.
  await client.prefetchQuery({
    queryKey: keys.hosts,
    queryFn: () => hosts,
  });
  render(
    <QueryClientProvider client={client}>
      <Dialog.Root open>
        <CreateFarmForm
          close={vi.fn()}
          onPending={vi.fn()}
          initialHost="target"
        />
      </Dialog.Root>
    </QueryClientProvider>,
  );
  const user = userEvent.setup();
  const select = screen.getByRole("combobox", { name: "실행 호스트" });
  expect(select.textContent).toContain("target · 원격");
  await user.click(select);
  expect(
    (
      await screen.findByRole("option", { name: "pending · 원격 · 준비 필요" })
    ).getAttribute("aria-disabled"),
  ).toBe("true");
  expect(
    (
      await screen.findByRole("option", {
        name: "old-agent · 원격 · 실행기 업데이트 필요",
      })
    ).getAttribute("aria-disabled"),
  ).toBe("true");
  await user.click(screen.getByRole("combobox", { name: "실행 호스트" }));
  await user.type(screen.getByPlaceholderText("dev-openstack"), "on-target");
  const submit = screen.getByRole("button", { name: "Farm 만들기" });
  await waitFor(() =>
    expect((submit as HTMLButtonElement).disabled).toBe(false),
  );
  await user.click(submit);
  await waitFor(() =>
    expect(submitted).toMatchObject({
      id: "on-target",
      host_id: "target",
      external_host: "10.0.0.21",
    }),
  );
  client.clear();
});

test("adds optional services with their dependencies and keeps unavailable ones locked", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  const component = (
    id: string,
    depends_on: string[],
    extra: { required?: boolean; available?: boolean } = {},
  ) => ({
    id,
    depends_on,
    required: false,
    available: true,
    requires_data_plane: false,
    ...extra,
  });
  const planned: string[][] = [];
  let submitted: { components?: string[] } | undefined;
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string, options?: RequestInit) => {
      if (path.endsWith("/catalog"))
        return Response.json({
          constraints: [],
          components: [
            component("keystone", [], { required: true }),
            component("glance", ["keystone"], { required: true }),
            component("placement", ["keystone"]),
            component("horizon", ["keystone"]),
            component("neutron", ["keystone"]),
            component("nova", ["keystone", "glance", "placement", "neutron"], {
              available: false,
            }),
          ],
        });
      if (path.endsWith("/plans")) {
        const selection = JSON.parse(String(options?.body)).components;
        planned.push(selection);
        return Response.json(
          singleVmPlan(selection.includes("horizon") ? 3584 : 3072),
        );
      }
      if (path.endsWith("/host"))
        return Response.json({
          msb_available: true,
          hypervisor_available: true,
          golden_image_available: true,
          memory_budget_mib: 16000,
          memory_reserved_mib: 0,
        });
      if (path.endsWith("/hosts"))
        return Response.json([
          {
            id: "local",
            kind: "local",
            address: "localhost",
            status: "ready",
            capacity: { memory_budget_mib: 16000, memory_reserved_mib: 0 },
          },
        ]);
      if (options?.method === "POST" && !path.includes("/releases/plan")) {
        submitted = JSON.parse(String(options.body));
        return Response.json({ id: "job", farm_id: "dash" });
      }
      if (path.endsWith("/releases")) return Response.json(releaseCatalog);
      if (path.endsWith("/releases/plan"))
        return Response.json(releasePreview(JSON.parse(String(options?.body))));
      return Response.json([]);
    }),
  );
  render(
    <QueryClientProvider client={client}>
      <Dialog.Root open>
        <CreateFarmForm close={vi.fn()} onPending={vi.fn()} />
      </Dialog.Root>
    </QueryClientProvider>,
  );
  const user = userEvent.setup();
  const box = async (name: string) =>
    (await screen.findByText(name))
      .closest("label")!
      .querySelector("[role=checkbox]")!;
  expect((await box("Keystone")).getAttribute("aria-disabled")).toBe("true");
  expect((await box("Nova")).getAttribute("aria-disabled")).toBe("true");
  expect((await box("Nova")).getAttribute("aria-checked")).toBe("false");
  expect((await box("Neutron")).getAttribute("aria-disabled")).toBe(null);
  await user.click(await box("Horizon"));
  expect((await box("Horizon")).getAttribute("aria-checked")).toBe("true");
  await waitFor(() =>
    expect(planned.at(-1)).toEqual(["keystone", "glance", "horizon"]),
  );
  expect(screen.getByText("3.5 GiB")).toBeTruthy();
  expect(
    screen.getByText("1", { selector: ".plan-summary strong" }),
  ).toBeTruthy();
  await user.type(screen.getByPlaceholderText("dev-openstack"), "dash");
  const submit = screen.getByRole("button", { name: "Farm 만들기" });
  await waitFor(() =>
    expect((submit as HTMLButtonElement).disabled).toBe(false),
  );
  await user.click(submit);
  await waitFor(() =>
    expect(submitted?.components).toEqual(["keystone", "glance", "horizon"]),
  );
  client.clear();
});

test("creates a control VM alone so computes can join it later", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  const planned: { components: string[]; computes: number; layout?: string }[] =
    [];
  let submitted: { computes?: number; layout?: string; components?: string[] } =
    {};
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string, options?: RequestInit) => {
      if (path.endsWith("/catalog"))
        return Response.json({
          constraints: [],
          components: [
            { id: "keystone", required: true, depends_on: [] },
            { id: "glance", required: true, depends_on: ["keystone"] },
            { id: "nova", depends_on: ["keystone"] },
          ],
        });
      if (path.endsWith("/plans")) {
        planned.push(JSON.parse(String(options?.body)));
        return Response.json(singleVmPlan());
      }
      if (path.endsWith("/host")) return Response.json({ build_id: "b" });
      if (path.endsWith("/hosts"))
        return Response.json([
          {
            id: "local",
            kind: "local",
            address: "localhost",
            status: "ready",
            capacity: { memory_budget_mib: 16000, memory_reserved_mib: 0 },
          },
        ]);
      if (options?.method === "POST" && !path.includes("/releases/plan")) {
        submitted = JSON.parse(String(options.body));
        return Response.json({ id: "job", farm_id: "ctl" });
      }
      if (path.endsWith("/releases")) return Response.json(releaseCatalog);
      if (path.endsWith("/releases/plan"))
        return Response.json(releasePreview(JSON.parse(String(options?.body))));
      return Response.json([]);
    }),
  );
  render(
    <QueryClientProvider client={client}>
      <Dialog.Root open>
        <CreateFarmForm close={vi.fn()} onPending={vi.fn()} />
      </Dialog.Root>
    </QueryClientProvider>,
  );
  const user = userEvent.setup();
  await user.click(await screen.findByRole("combobox", { name: "VM 구성" }));
  await user.click(
    await screen.findByRole("option", { name: "Control만 · compute는 나중에" }),
  );
  // Nova's control plane is installed now, its compute later.
  await waitFor(() =>
    expect(planned.at(-1)).toMatchObject({ computes: 0, layout: "control" }),
  );
  expect(planned.at(-1)?.components).toContain("nova");

  await user.type(screen.getByPlaceholderText("dev-openstack"), "ctl");
  const submit = screen.getByRole("button", { name: "Farm 만들기" });
  await waitFor(() =>
    expect((submit as HTMLButtonElement).disabled).toBe(false),
  );
  await user.click(submit);
  await waitFor(() =>
    expect(submitted).toMatchObject({
      id: "ctl",
      computes: 0,
      layout: "control",
    }),
  );
  client.clear();
});

test("asks to register a host when there is none", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string, options?: RequestInit) => {
      if (path.endsWith("/catalog"))
        return Response.json({ components: [{ id: "keystone" }] });
      if (path.endsWith("/plans")) return Response.json(singleVmPlan());
      if (path.endsWith("/host")) return Response.json({ build_id: "x" });
      if (path.endsWith("/releases")) return Response.json(releaseCatalog);
      if (path.endsWith("/releases/plan"))
        return Response.json(releasePreview(JSON.parse(String(options?.body))));
      return Response.json([]);
    }),
  );
  render(
    <QueryClientProvider client={client}>
      <Dialog.Root open>
        <CreateFarmForm close={vi.fn()} onPending={vi.fn()} />
      </Dialog.Root>
    </QueryClientProvider>,
  );
  await screen.findByText(/등록된 호스트가 없습니다/);
  const user = userEvent.setup();
  await user.type(screen.getByPlaceholderText("dev-openstack"), "nowhere");
  expect(
    (screen.getByRole("button", { name: "Farm 만들기" }) as HTMLButtonElement)
      .disabled,
  ).toBe(true);
  client.clear();
});

function singleVmPlan(memory_mib = 3072): Plan {
  return {
    memory_mib,
    disk_mib: 86016,
    components: ["keystone", "glance"],
    warnings: [],
    nodes: [
      {
        role: "all-in-one",
        cpus: 2,
        disk_mib: 20480,
        container_disk_mib: 32768,
        memory_mib,
      },
    ],
  };
}

test("allows deployment only for a running all-in-one VM and preserves legacy farms", () => {
  const client = new QueryClient();
  const single: Farm = {
    id: "single",
    created_at: 0,
    updated_at: 0,
    status: "created",
    active_job: null,
    plan: singleVmPlan(),
    nodes: [
      {
        name: "single-all-in-one",
        role: "all-in-one",
        status: "running",
        memory_mib: 3072,
      },
    ],
  };
  const legacy: Farm = {
    ...single,
    id: "legacy",
    plan: {
      ...singleVmPlan(),
      nodes: [
        { ...singleVmPlan().nodes[0], role: "deployer" },
        { ...singleVmPlan().nodes[0], role: "infra" },
      ],
    },
    nodes: [
      {
        name: "legacy-deployer",
        role: "deployer",
        status: "running",
        memory_mib: 1024,
      },
      {
        name: "legacy-infra",
        role: "infra",
        status: "running",
        memory_mib: 2048,
      },
    ],
  };
  const view = render(
    <QueryClientProvider client={client}>
      <DeployFarmDialog farm={single} />
    </QueryClientProvider>,
  );
  expect(
    (
      screen.getByRole("button", {
        name: "OpenStack 배포",
      }) as HTMLButtonElement
    ).disabled,
  ).toBe(false);
  view.rerender(
    <QueryClientProvider client={client}>
      <DeployFarmDialog farm={legacy} />
    </QueryClientProvider>,
  );
  const trigger = screen.getByRole("button", {
    name: "OpenStack 배포",
  }) as HTMLButtonElement;
  expect(trigger.disabled).toBe(true);
  expect(trigger.title).toContain("Farm을 새로 만드세요");
  // A control VM and its computes deploy once all of them run.
  const spec = singleVmPlan().nodes[0];
  const multi: Farm = {
    ...single,
    id: "mn",
    plan: {
      ...singleVmPlan(),
      computes: 2,
      nodes: [
        { ...spec, role: "control" },
        { ...spec, role: "compute", index: 1 },
        { ...spec, role: "compute", index: 2 },
      ],
    },
    nodes: [
      {
        name: "mn-control",
        role: "control",
        status: "running",
        memory_mib: 6656,
      },
      {
        name: "mn-compute-1",
        role: "compute",
        index: 1,
        status: "running",
        memory_mib: 3072,
      },
      {
        name: "mn-compute-2",
        role: "compute",
        index: 2,
        status: "running",
        memory_mib: 3072,
      },
    ],
  };
  const deployButton = () =>
    screen.getByRole("button", { name: "OpenStack 배포" }) as HTMLButtonElement;
  view.rerender(
    <QueryClientProvider client={client}>
      <DeployFarmDialog farm={multi} />
    </QueryClientProvider>,
  );
  expect(deployButton().disabled).toBe(false);
  view.rerender(
    <QueryClientProvider client={client}>
      <DeployFarmDialog farm={{ ...multi, nodes: multi.nodes.slice(0, 2) }} />
    </QueryClientProvider>,
  );
  expect(deployButton().disabled).toBe(true);
  client.clear();
});

test("adds the next compute to the farm's primary host by default", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  const user = userEvent.setup();
  let submitted: unknown;
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string, options?: RequestInit) => {
      if (path.endsWith("/hosts"))
        return Response.json([
          {
            id: "local",
            kind: "local",
            address: "localhost",
            status: "ready",
            inspection: { checked_at: Date.now() / 1000 },
            capacity: { memory_budget_mib: 8000, memory_reserved_mib: 0 },
          },
        ]);
      if (path.endsWith("/host"))
        return Response.json({
          msb_available: true,
          memory_budget_mib: 0,
          memory_reserved_mib: 0,
          relay_url: null,
        });
      if (path.endsWith("/computes") && options?.method === "POST") {
        submitted = JSON.parse(String(options.body));
        return Response.json({
          id: "job",
          farm_id: "grow",
          kind: "expand",
          status: "queued",
        });
      }
      if (path.endsWith("/releases")) return Response.json(releaseCatalog);
      if (path.endsWith("/releases/plan"))
        return Response.json(releasePreview(JSON.parse(String(options?.body))));
      return Response.json([]);
    }),
  );
  const spec = singleVmPlan().nodes[0];
  const farm: Farm = {
    id: "grow",
    host_id: "local",
    created_at: 0,
    updated_at: 0,
    status: "ready",
    active_job: null,
    plan: {
      ...singleVmPlan(),
      computes: 1,
      nodes: [
        { ...spec, role: "control" },
        { ...spec, role: "compute", index: 1 },
      ],
    },
    nodes: [
      {
        name: "grow-control",
        role: "control",
        status: "running",
        memory_mib: 6656,
      },
      {
        name: "grow-compute-1",
        role: "compute",
        index: 1,
        status: "running",
        memory_mib: 3072,
      },
    ],
  };
  render(
    <QueryClientProvider client={client}>
      <AddComputeDialog farm={farm} />
    </QueryClientProvider>,
  );
  await user.click(screen.getByRole("button", { name: "Compute 추가" }));
  await user.click(await screen.findByRole("button", { name: "추가" }));
  await waitFor(() => expect(submitted).toEqual({ host_id: "local" }));
  cleanup();
});

/** A deployed farm with a control VM and one compute on the local host. */
function grownFarm(): Farm {
  const spec = singleVmPlan().nodes[0];
  return {
    id: "grow",
    host_id: "local",
    created_at: 0,
    updated_at: 0,
    status: "ready",
    active_job: null,
    exposure: { host: "localhost", bind: "127.0.0.1", port_base: 15000 },
    plan: {
      ...singleVmPlan(),
      computes: 1,
      nodes: [
        { ...spec, role: "control" },
        { ...spec, role: "compute", index: 1 },
      ],
    },
    nodes: [
      {
        name: "grow-control",
        role: "control",
        status: "running",
        memory_mib: 6656,
      },
      {
        name: "grow-compute-1",
        role: "compute",
        index: 1,
        status: "running",
        memory_mib: 3072,
      },
    ],
  };
}
/** Answers the portal's reads around a farm's actions, and records the POST to `post`. */
function stubFarmApi(post: string, snapshots: unknown[] = []) {
  const posted: { body?: unknown } = {};
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string, options?: RequestInit) => {
      if (path.endsWith("/hosts"))
        return Response.json([
          {
            id: "local",
            kind: "local",
            address: "localhost",
            status: "ready",
            inspection: { checked_at: Date.now() / 1000 },
            capacity: { memory_budget_mib: 8000, memory_reserved_mib: 0 },
          },
        ]);
      if (path.endsWith("/host"))
        return Response.json({
          msb_available: true,
          memory_budget_mib: 0,
          memory_reserved_mib: 0,
          relay_url: null,
        });
      if (path.endsWith("/snapshots") && options?.method !== "POST")
        return Response.json(snapshots);
      if (path.endsWith(post) && options?.method === "POST") {
        posted.body = JSON.parse(String(options.body));
        return Response.json({ id: "job", farm_id: "grow", status: "queued" });
      }
      if (path.endsWith("/releases")) return Response.json(releaseCatalog);
      if (path.endsWith("/releases/plan"))
        return Response.json(releasePreview(JSON.parse(String(options?.body))));
      return Response.json([]);
    }),
  );
  return posted;
}

test("adds a compute from a compatible compute template", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  const user = userEvent.setup();
  const plan = { ...singleVmPlan(), computes: 1 };
  const posted = stubFarmApi("/computes", [
    { id: "whole", status: "ready", arch: "aarch64", plan },
    {
      id: "other-services",
      status: "ready",
      arch: "aarch64",
      template: "compute",
      plan: { ...plan, components: ["keystone", "glance", "horizon"] },
    },
    {
      id: "control-tpl",
      status: "ready",
      arch: "aarch64",
      template: "control",
      plan,
    },
    {
      id: "tpl",
      status: "ready",
      arch: "aarch64",
      template: "compute",
      release: "2026.2",
      plan,
    },
  ]);
  render(
    <QueryClientProvider client={client}>
      <AddComputeDialog farm={grownFarm()} />
    </QueryClientProvider>,
  );
  await user.click(screen.getByRole("button", { name: "Compute 추가" }));
  await user.click(await screen.findByRole("radio", { name: "스냅샷에서" }));
  const select = await screen.findByRole("combobox", {
    name: "compute 템플릿 스냅샷",
  });
  await waitFor(() => expect(select.textContent).toContain("tpl"));
  await user.click(select);
  const options = await screen.findAllByRole("option");
  expect(options.map((option) => option.textContent)).toEqual(["tpl"]);
  await user.keyboard("{Escape}");
  await user.click(screen.getByRole("button", { name: "추가" }));
  await waitFor(() =>
    expect(posted.body).toEqual({ host_id: "local", from_snapshot: "tpl" }),
  );
  cleanup();
});

test("freezes one compute VM of a deployed farm as a compute template", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  const user = userEvent.setup();
  const posted = stubFarmApi("/farms/grow/snapshots");
  render(
    <QueryClientProvider client={client}>
      <FreezeFarmDialog farm={grownFarm()} />
    </QueryClientProvider>,
  );
  const trigger = screen.getByRole("button", { name: "얼리기" });
  await waitFor(() =>
    expect((trigger as HTMLButtonElement).disabled).toBe(false),
  );
  await user.click(trigger);
  await user.click(await screen.findByRole("combobox", { name: "얼릴 대상" }));
  await user.click(
    await screen.findByRole("option", { name: "compute-1 (compute 템플릿)" }),
  );
  await user.click(screen.getByRole("button", { name: "얼리기 시작" }));
  await waitFor(() =>
    expect(posted.body).toEqual({
      id: "grow-compute-1-tpl",
      node: "compute-1",
    }),
  );
  cleanup();
});

/** A farm made from a control template: its control VM alone, before any compute. */
function controlOnlyFarm(): Farm {
  const farm = grownFarm();
  return {
    ...farm,
    snapshot_id: "control-tpl",
    plan: { ...farm.plan!, computes: 0, nodes: farm.plan!.nodes.slice(0, 1) },
    nodes: farm.nodes.slice(0, 1),
  };
}

test("freezes the control VM of a deployed farm as a control template", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  const user = userEvent.setup();
  const posted = stubFarmApi("/farms/grow/snapshots");
  render(
    <QueryClientProvider client={client}>
      <FreezeFarmDialog farm={grownFarm()} />
    </QueryClientProvider>,
  );
  const trigger = screen.getByRole("button", { name: "얼리기" });
  await waitFor(() =>
    expect((trigger as HTMLButtonElement).disabled).toBe(false),
  );
  await user.click(trigger);
  await user.click(await screen.findByRole("combobox", { name: "얼릴 대상" }));
  const options = await screen.findAllByRole("option");
  expect(options.map((option) => option.textContent)).toEqual([
    "farm 전체",
    "control (control 템플릿)",
    "compute-1 (compute 템플릿)",
  ]);
  await user.click(
    screen.getByRole("option", { name: "control (control 템플릿)" }),
  );
  expect(
    screen.getByText(/control VM 하나를 control 템플릿으로 저장합니다/),
  ).toBeTruthy();
  await user.click(screen.getByRole("button", { name: "얼리기 시작" }));
  await waitFor(() =>
    expect(posted.body).toEqual({ id: "grow-control-tpl", node: "control" }),
  );
  cleanup();
});

test("a farm without computes is frozen as a control template only", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  const user = userEvent.setup();
  const posted = stubFarmApi("/farms/grow/snapshots");
  render(
    <QueryClientProvider client={client}>
      <FreezeFarmDialog farm={controlOnlyFarm()} />
    </QueryClientProvider>,
  );
  const trigger = screen.getByRole("button", { name: "얼리기" });
  await waitFor(() =>
    expect((trigger as HTMLButtonElement).disabled).toBe(false),
  );
  await user.click(trigger);
  await user.click(await screen.findByRole("combobox", { name: "얼릴 대상" }));
  const options = await screen.findAllByRole("option");
  expect(options.map((option) => option.textContent)).toEqual([
    "control (control 템플릿)",
  ]);
  await user.keyboard("{Escape}");
  await user.click(screen.getByRole("button", { name: "얼리기 시작" }));
  await waitFor(() =>
    expect(posted.body).toEqual({ id: "grow-control-tpl", node: "control" }),
  );
  cleanup();
});

test("a farm made from a control template gains computes instead of a redeploy", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  const user = userEvent.setup();
  const plan = { ...singleVmPlan(), computes: 1 };
  const posted = stubFarmApi("/computes", [
    {
      id: "tpl",
      status: "ready",
      arch: "aarch64",
      template: "compute",
      release: "2026.2",
      plan,
    },
  ]);
  const farm = controlOnlyFarm();
  render(
    <QueryClientProvider client={client}>
      <DeployFarmDialog farm={farm} />
      <AddComputeDialog farm={farm} />
    </QueryClientProvider>,
  );
  const deploy = screen.getByRole("button", { name: "다시 배포" });
  expect((deploy as HTMLButtonElement).disabled).toBe(true);
  expect(deploy.getAttribute("title")).toContain("Compute를 추가하세요");
  const add = screen.getByRole("button", { name: "Compute 추가" });
  expect((add as HTMLButtonElement).disabled).toBe(false);
  await user.click(add);
  expect(await screen.findByText("Compute 1 추가")).toBeTruthy();
  await user.click(await screen.findByRole("radio", { name: "스냅샷에서" }));
  const select = await screen.findByRole("combobox", {
    name: "compute 템플릿 스냅샷",
  });
  await waitFor(() => expect(select.textContent).toContain("tpl"));
  await user.click(screen.getByRole("button", { name: "추가" }));
  await waitFor(() =>
    expect(posted.body).toEqual({ host_id: "local", from_snapshot: "tpl" }),
  );
  cleanup();
});

test("starts a farm from a control template through the restore dialog", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  const user = userEvent.setup();
  const posted = stubFarmApi("/farms");
  const plan = controlOnlyFarm().plan!;
  render(
    <QueryClientProvider client={client}>
      <RestoreSnapshotDialog
        snapshot={
          {
            id: "control-tpl",
            status: "ready",
            arch: "aarch64",
            template: "control",
            deployed: true,
            plan,
          } as Snapshot
        }
      />
    </QueryClientProvider>,
  );
  await user.click(screen.getByRole("button", { name: "Farm 만들기" }));
  expect(
    await screen.findByText("Control 템플릿에서 farm 만들기"),
  ).toBeTruthy();
  expect(screen.getByText(/control VM만, compute 0대/)).toBeTruthy();
  await user.type(screen.getByPlaceholderText("dev-openstack"), "cluster");
  // The dialog's submit button carries the trigger's name.
  const submit = screen.getAllByRole("button", { name: "Farm 만들기" }).at(-1)!;
  await waitFor(() =>
    expect((submit as HTMLButtonElement).disabled).toBe(false),
  );
  await user.click(submit);
  await waitFor(() =>
    expect(posted.body).toEqual({
      id: "cluster",
      host_id: "local",
      snapshot_id: "control-tpl",
      external_host: "localhost",
    }),
  );
  cleanup();
});

test("deletes only the records when asked, without the host", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  const deletes: string[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string, options?: RequestInit) => {
      if (options?.method === "DELETE") {
        deletes.push(path);
        return Response.json({ id: "x" }, { status: 202 });
      }
      if (path.endsWith("/releases")) return Response.json(releaseCatalog);
      if (path.endsWith("/releases/plan"))
        return Response.json(releasePreview(JSON.parse(String(options?.body))));
      return Response.json([]);
    }),
  );
  render(
    <QueryClientProvider client={client}>
      <DeleteFarmDialog
        farm={{ id: "dev", status: "failed", host_id: "gone" } as Farm}
      />
      <DeleteSnapshotDialog
        snapshot={{ id: "clean", status: "ready" } as Snapshot}
      />
    </QueryClientProvider>,
  );
  const user = userEvent.setup();

  await user.click(screen.getByRole("button", { name: "Farm 삭제" }));
  await user.click(screen.getByRole("checkbox", { name: /기록만 삭제/ }));
  await user.type(screen.getByLabelText("확인을 위해 dev 입력"), "dev");
  await user.click(screen.getByRole("button", { name: "기록만 삭제" }));
  await waitFor(() =>
    expect(deletes).toEqual(["/api/v1/farms/dev?forget=true"]),
  );

  await user.click(screen.getByRole("button", { name: "clean 스냅샷 삭제" }));
  // The option starts off each time a dialog opens.
  const forget = screen.getByRole("checkbox", { name: /기록만 삭제/ });
  expect(forget.getAttribute("aria-checked")).toBe("false");
  await user.click(screen.getByRole("button", { name: "스냅샷 삭제" }));
  await user.click(screen.getByRole("button", { name: "clean 스냅샷 삭제" }));
  await user.click(screen.getByRole("checkbox", { name: /기록만 삭제/ }));
  await user.click(screen.getByRole("button", { name: "기록만 삭제" }));
  await waitFor(() =>
    expect(deletes).toEqual([
      "/api/v1/farms/dev?forget=true",
      "/api/v1/snapshots/clean",
      "/api/v1/snapshots/clean?forget=true",
    ]),
  );
  client.clear();
});

test("places compute VMs on other hosts when the coordinator has a relay", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  let submitted: unknown;
  const node = singleVmPlan().nodes[0];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string, options?: RequestInit) => {
      if (path.endsWith("/catalog"))
        return Response.json({
          components: [{ id: "keystone" }, { id: "nova" }],
        });
      if (path.endsWith("/plans"))
        return Response.json({
          ...singleVmPlan(9728),
          computes: 1,
          nodes: [
            { ...node, role: "control", memory_mib: 6656 },
            { ...node, role: "compute", index: 1, memory_mib: 3072 },
          ],
        });
      if (path.endsWith("/host"))
        return Response.json({ relay_url: "https://10.0.0.5:10011" });
      if (path.endsWith("/hosts"))
        return Response.json(
          ["local", "edge"].map((id) => ({
            id,
            kind: "local",
            address: "localhost",
            status: "ready",
            // The control VM's host alone could not hold the whole farm.
            capacity: { memory_budget_mib: 8000, memory_reserved_mib: 0 },
          })),
        );
      if (options?.method === "POST" && !path.includes("/releases/plan")) {
        submitted = JSON.parse(String(options.body));
        return Response.json({ id: "job", farm_id: "span", status: "queued" });
      }
      if (path.endsWith("/releases")) return Response.json(releaseCatalog);
      if (path.endsWith("/releases/plan"))
        return Response.json(releasePreview(JSON.parse(String(options?.body))));
      return Response.json([]);
    }),
  );
  render(
    <QueryClientProvider client={client}>
      <Dialog.Root open>
        <CreateFarmForm close={vi.fn()} onPending={vi.fn()} />
      </Dialog.Root>
    </QueryClientProvider>,
  );
  const user = userEvent.setup();
  await user.type(screen.getByPlaceholderText("dev-openstack"), "span");
  await user.click(screen.getByRole("combobox", { name: "VM 구성" }));
  await user.click(
    await screen.findByRole("option", {
      name: "Control + Compute 1대",
    }),
  );
  await user.click(
    await screen.findByRole("combobox", { name: "compute-1 호스트" }),
  );
  await user.click(
    await screen.findByRole("option", { name: /compute-1 · edge/ }),
  );
  // The control VM's host is chosen beside the computes, the same as 실행 호스트.
  await user.click(screen.getByRole("combobox", { name: "control 호스트" }));
  await user.click(
    await screen.findByRole("option", { name: /control · edge/ }),
  );
  await user.click(screen.getByRole("combobox", { name: "compute-1 호스트" }));
  await user.click(
    await screen.findByRole("option", { name: /compute-1 · local/ }),
  );
  const create = screen.getByRole("button", {
    name: "Farm 만들기",
  }) as HTMLButtonElement;
  await waitFor(() => expect(create.disabled).toBe(false));
  await user.click(create);
  await waitFor(() =>
    expect(submitted).toMatchObject({
      id: "span",
      host_id: "edge",
      computes: 1,
      compute_hosts: ["local"],
    }),
  );
  client.clear();
});
