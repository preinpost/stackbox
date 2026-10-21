import { Dialog } from "@base-ui/react/dialog";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, cleanup, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, test, vi } from "vitest";
import { keys, type Farm, type Plan } from "../api/client";
import { CreateFarmForm, DeployFarmDialog } from "./farm-actions";

const { navigate } = vi.hoisted(() => ({
  navigate: vi.fn().mockResolvedValue(undefined),
}));
vi.mock("@tanstack/react-router", () => ({ useNavigate: () => navigate }));
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

test("uses the selected remote host's capacity and sends its ID and public hostname", async () => {
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
            id: "target",
            address: "10.0.0.21",
            status: "ready",
            agent: { artifact: { supports_single_vm: true } },
            inspection: { checked_at: Date.now() / 1000 },
            capacity: { memory_budget_mib: 8000, memory_reserved_mib: 0 },
          },
        ]);
      if (options?.method === "POST") {
        submitted = JSON.parse(String(options.body));
        return Response.json({
          id: "job",
          farm_id: "remote-test",
          status: "queued",
        });
      }
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
  await screen.findByText(
    "Hosts의 로컬 호스트에서 msb·가상화·golden image 상태를 확인하세요.",
  );
  expect(
    (screen.getByRole("button", { name: "Farm 만들기" }) as HTMLButtonElement)
      .disabled,
  ).toBe(true);
  await user.click(screen.getByRole("combobox", { name: "실행 호스트" }));
  await user.click(
    await screen.findByRole("option", { name: "target · 원격 실행" }),
  );
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
      id: "target",
      address: "10.0.0.21",
      status: "ready",
      agent: { artifact: { supports_single_vm: true } },
      inspection: checked,
      capacity: { memory_budget_mib: 8000, memory_reserved_mib: 0 },
    },
    {
      id: "old-agent",
      address: "10.0.0.23",
      status: "ready",
      agent: { artifact: {} },
      inspection: checked,
      capacity: { memory_budget_mib: 8000, memory_reserved_mib: 0 },
    },
    {
      id: "pending",
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
      if (options?.method === "POST") {
        submitted = JSON.parse(String(options.body));
        return Response.json({ id: "job", farm_id: "on-target" });
      }
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
          initialTarget="remote:target"
        />
      </Dialog.Root>
    </QueryClientProvider>,
  );
  const user = userEvent.setup();
  const select = screen.getByRole("combobox", { name: "실행 호스트" });
  expect(select.textContent).toContain("target · 원격 실행");
  await user.click(select);
  expect(
    (
      await screen.findByRole("option", { name: "pending · 준비 필요" })
    ).getAttribute("aria-disabled"),
  ).toBe("true");
  expect(
    (
      await screen.findByRole("option", {
        name: "old-agent · 실행기 업데이트 필요",
      })
    ).getAttribute("aria-disabled"),
  ).toBe("true");
  await user.keyboard("{Escape}");
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
      if (options?.method === "POST") {
        submitted = JSON.parse(String(options.body));
        return Response.json({ id: "job", farm_id: "dash" });
      }
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
  expect(trigger.title).toContain("단일 VM Farm을 새로 만드세요");
  client.clear();
});
