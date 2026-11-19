import { Dialog } from "@base-ui/react/dialog";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, test, vi } from "vitest";
import type { Job, ReleasePlan } from "../api/client";
import { activityStep } from "../pages/jobs-model";
import { AssemblySteps } from "../pages/jobs-monitor";
import { CreateFarmForm, bucketIssues } from "./farm-actions";
import { releaseLabel } from "./release-create";

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
  navigate.mockClear();
});

const ready = {
  status: "ready",
  agent: { artifact: { supports_single_vm: true } },
  inspection: { checked_at: Date.now() / 1000 },
  capacity: { memory_budget_mib: 64000, memory_reserved_mib: 0 },
};
const hosts = [
  { id: "edge", kind: "ssh", address: "10.0.0.21", ...ready },
  { id: "local", kind: "local", address: "localhost", ...ready },
];
const catalog = {
  constraints: [],
  components: [
    { id: "keystone", required: true, depends_on: [] },
    { id: "glance", required: true, depends_on: ["keystone"] },
  ],
};
const resourcePlan = {
  memory_mib: 3072,
  disk_mib: 86016,
  components: ["keystone", "glance"],
  warnings: [],
  nodes: [
    {
      role: "all-in-one",
      cpus: 2,
      disk_mib: 20480,
      container_disk_mib: 32768,
      memory_mib: 3072,
    },
  ],
};

const plan = (computes: string[]): ReleasePlan => ({
  farm_id: "r",
  release: "2026.2",
  arch: "aarch64",
  components: ["keystone", "glance", "nova"],
  control: {
    node: "control",
    host_id: "local",
    source: "template",
    snapshot_id: "ctl",
    reason: "the newest of 2 control templates of OpenStack 2026.2 kept here",
  },
  computes: computes.map((host, i) =>
    host === "local"
      ? {
          node: `compute-${i + 1}`,
          host_id: host,
          source: "bucket",
          snapshot_id: "cpt",
          bucket: "minio",
          key: "snapshots/cpt.msb",
          reason: "the compute template of OpenStack 2026.2 in buckets",
        }
      : {
          node: `compute-${i + 1}`,
          host_id: host,
          source: "clean",
          reason: `templates are not copied to host '${host}' yet`,
        },
  ),
  notes: [],
});

test("previews each VM's source for the chosen release and starts the plan", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  const previews: Record<string, unknown>[] = [];
  let started: unknown;
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string, options?: RequestInit) => {
      if (path.endsWith("/releases"))
        return Response.json({
          arch: "aarch64",
          releases: [
            {
              release: "2026.2",
              local_control: 2,
              local_compute: 0,
              bucket_control: 0,
              bucket_compute: 1,
              installable: true,
            },
            {
              release: "2025.1",
              local_control: 0,
              local_compute: 0,
              bucket_control: 1,
              bucket_compute: 0,
              installable: false,
            },
          ],
          unreachable: [],
        });
      if (path.endsWith("/releases/plan")) {
        const body = JSON.parse(String(options?.body));
        previews.push(body);
        return Response.json(
          plan(
            Array.from(
              { length: body.computes },
              (_, i) => body.compute_hosts[i] || body.host_id,
            ),
          ),
        );
      }
      if (path.endsWith("/releases/farms")) {
        started = JSON.parse(String(options?.body));
        return Response.json({
          id: "job",
          farm_id: "r",
          kind: "assemble",
          status: "queued",
        });
      }
      if (path.includes("/catalog")) return Response.json(catalog);
      if (path.endsWith("/plans")) return Response.json(resourcePlan);
      if (path.endsWith("/host"))
        return Response.json({
          build_id: "b",
          relay_url: "https://relay.test",
        });
      if (path.endsWith("/hosts")) return Response.json(hosts);
      return Response.json([]);
    }),
  );
  const close = vi.fn();
  render(
    <QueryClientProvider client={client}>
      <Dialog.Root open>
        <CreateFarmForm close={close} onPending={vi.fn()} />
      </Dialog.Root>
    </QueryClientProvider>,
  );
  const user = userEvent.setup();
  // The release with control templates, and the local host that keeps them, come first.
  const release = await screen.findByRole("combobox", {
    name: "OpenStack 릴리스",
  });
  await waitFor(() => expect(release.textContent).toContain("2026.2"));
  expect(
    screen.getByRole("combobox", { name: "실행 호스트" }).textContent,
  ).toContain("local");
  expect(screen.getByText(/Farm ID를 입력하면/)).toBeTruthy();
  await user.type(screen.getByPlaceholderText("dev-openstack"), "r");
  await screen.findByText("템플릿 ctl");

  // A second compute on another host is installed clean, and the preview says why.
  await user.click(screen.getByRole("combobox", { name: "VM 구성" }));
  await user.click(
    await screen.findByRole("option", { name: "Control + Compute 2대" }),
  );
  await user.click(screen.getByRole("combobox", { name: "compute-2 호스트" }));
  await user.click(
    await screen.findByRole("option", { name: "compute-2 · edge · 원격" }),
  );
  await screen.findByText("클린 설치");
  // The compute on the local host comes from a compute template in a bucket.
  expect(
    screen.getByText(/버킷 minio의 snapshots\/cpt.msb → cpt로 가져와 사용/),
  ).toBeTruthy();
  expect(screen.getByText(/not copied to host 'edge'/)).toBeTruthy();
  expect(previews.at(-1)).toMatchObject({
    id: "r",
    release: "2026.2",
    host_id: "local",
    computes: 2,
    compute_hosts: ["", "edge"],
  });

  const start = screen.getByRole("button", { name: "Farm 만들기" });
  await waitFor(() =>
    expect((start as HTMLButtonElement).disabled).toBe(false),
  );
  await user.click(start);
  await waitFor(() =>
    expect(started).toMatchObject({
      id: "r",
      release: "2026.2",
      computes: 2,
      compute_hosts: ["", "edge"],
    }),
  );
  // The dialog closes on the farm it made, whose page shows the assemble job from its first
  // step, instead of leaving the reader on the jobs list.
  await waitFor(() =>
    expect(navigate).toHaveBeenCalledWith({
      to: "/farms/$farmId",
      params: { farmId: "r" },
    }),
  );
  expect(close).toHaveBeenCalled();
  client.clear();
});

const renderForm = (client: QueryClient) =>
  render(
    <QueryClientProvider client={client}>
      <Dialog.Root open>
        <CreateFarmForm close={vi.fn()} onPending={vi.fn()} />
      </Dialog.Root>
    </QueryClientProvider>,
  );

test("a new farm is a control VM with one compute by default", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  const previews: Record<string, unknown>[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string, options?: RequestInit) => {
      if (path.endsWith("/releases"))
        return Response.json({
          arch: "aarch64",
          releases: [
            {
              release: "2026.2",
              local_control: 2,
              local_compute: 1,
              bucket_control: 0,
              bucket_compute: 0,
              installable: true,
            },
          ],
          unreachable: [],
        });
      if (path.endsWith("/releases/plan")) {
        const body = JSON.parse(String(options?.body));
        previews.push(body);
        return Response.json({
          farm_id: body.id,
          release: "2026.2",
          arch: "aarch64",
          components: ["keystone", "glance"],
          control: {
            node: "control",
            host_id: "local",
            source: "clean",
            reason: "installed clean with OpenStack 2026.2",
          },
          computes: Array.from({ length: body.computes }, (_, i) => ({
            node: `compute-${i + 1}`,
            host_id: body.compute_hosts[i] || "local",
            source: "clean",
            reason: "installed clean with OpenStack 2026.2",
          })),
          notes: [],
        });
      }
      if (path.includes("/catalog")) return Response.json(catalog);
      if (path.endsWith("/plans")) return Response.json(resourcePlan);
      if (path.endsWith("/host")) return Response.json({ build_id: "b" });
      if (path.endsWith("/hosts")) return Response.json(hosts);
      return Response.json([]);
    }),
  );
  renderForm(client);
  const user = userEvent.setup();
  await user.type(screen.getByPlaceholderText("dev-openstack"), "ctl");
  // One compute VM joins the control VM by default, and no legacy layout rides along.
  await waitFor(() => expect(previews.at(-1)).toMatchObject({ computes: 1 }));
  expect(previews.at(-1)).not.toHaveProperty("layout");
  await screen.findByText("control", { selector: ".plan-summary strong" });

  // All-in-one and control-only are gone; only the computes are chosen.
  await user.click(screen.getByRole("combobox", { name: "VM 구성" }));
  const options = (await screen.findAllByRole("option")).map((option) =>
    option.textContent?.trim(),
  );
  expect(options).toEqual([
    "Control + Compute 1대",
    "Control + Compute 2대",
    "Control + Compute 3대",
  ]);
  await user.click(
    screen.getByRole("option", { name: "Control + Compute 2대" }),
  );
  await waitFor(() => expect(previews.at(-1)).toMatchObject({ computes: 2 }));
  client.clear();
});

test("a release this coordinator cannot install without templates is refused in the preview", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string) => {
      if (path.endsWith("/releases"))
        return Response.json({
          arch: "aarch64",
          releases: [
            {
              release: "2025.1",
              local_control: 0,
              local_compute: 1,
              bucket_control: 0,
              bucket_compute: 0,
              installable: false,
            },
          ],
          unreachable: ["minio: connection refused"],
        });
      if (path.endsWith("/releases/plan"))
        return Response.json(
          {
            error:
              "no control template of OpenStack 2025.1 for aarch64; this coordinator installs only OpenStack 2026.2 clean",
          },
          { status: 409 },
        );
      if (path.includes("/catalog")) return Response.json(catalog);
      if (path.endsWith("/plans")) return Response.json(resourcePlan);
      if (path.endsWith("/host"))
        return Response.json({
          build_id: "b",
          relay_url: "https://relay.test",
        });
      if (path.endsWith("/hosts")) return Response.json(hosts);
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
  // The failure stays one short line, and the coordinator's own error waits in the log.
  await screen.findByText(/버킷 1개를 조회하지 못했습니다/);
  expect(
    screen.getByText("minio", { selector: ".bucket-notice-list code" }),
  ).toBeTruthy();
  expect(
    screen.getByText("연결이 거부되었습니다", {
      selector: ".bucket-notice-list span",
    }),
  ).toBeTruthy();
  expect(screen.getByText("원문 로그 1건")).toBeTruthy();
  await user.type(screen.getByPlaceholderText("dev-openstack"), "old");
  await screen.findByText(/installs only OpenStack 2026.2 clean/);
  expect(
    (
      screen.getByRole("button", {
        name: "Farm 만들기",
      }) as HTMLButtonElement
    ).disabled,
  ).toBe(true);
  client.clear();
});

test("turns a bucket's transport error into one line and keeps the log", () => {
  const [dns] = bucketIssues([
    "INNO: GET /abc: error sending request for url (https://s3.example.com/abc?list-type=2&max-keys=1000): client error (Connect): dns error: failed to lookup address information: nodename nor servname provided, or not known",
  ]);
  expect(dns.bucket).toBe("INNO");
  expect(dns.reason).toBe("주소를 찾지 못했습니다 (DNS 조회 실패)");
  expect(dns.detail).toMatch(/^GET \/abc/);
  // The plan route repeats a failure as a note, and a bare reason has no bucket to name.
  expect(
    bucketIssues(["bucket not searched: minio: connection refused"]),
  ).toEqual([
    {
      bucket: "minio",
      reason: "연결이 거부되었습니다",
      detail: "connection refused",
    },
  ]);
  // A transport error that starts the string is not a bucket name, so it gets none.
  expect(
    bucketIssues(["dial tcp 10.0.0.5:9000: i/o timeout"])[0],
  ).toMatchObject({
    bucket: "",
    reason: "시간 안에 응답하지 않았습니다",
  });
});

test("disables a release whose images miss the chosen host's architecture", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  const hostsByArch = [
    {
      id: "amd",
      kind: "ssh",
      address: "10.0.0.21",
      ...ready,
      inspection: { ...ready.inspection, arch: "x86_64" },
    },
    {
      id: "local",
      kind: "local",
      address: "localhost",
      ...ready,
      inspection: { ...ready.inspection, arch: "aarch64" },
    },
  ];
  const requested: string[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string) => {
      requested.push(path);
      if (path.includes("/releases")) {
        const arch = path.includes("?") ? path.split("arch=")[1] : "x86_64";
        return Response.json({
          arch,
          releases: [
            {
              release: "2026.2",
              local_control: 2,
              local_compute: 0,
              bucket_control: 0,
              bucket_compute: 0,
              installable: true,
              arches: ["x86_64", "aarch64"],
            },
            {
              release: "2024.1",
              local_control: 1,
              local_compute: 0,
              bucket_control: 0,
              bucket_compute: 0,
              installable: true,
              arches: ["x86_64"],
            },
          ],
          unreachable: [],
        });
      }
      if (path.includes("/catalog")) return Response.json(catalog);
      if (path.endsWith("/plans")) return Response.json(resourcePlan);
      if (path.endsWith("/host"))
        return Response.json({
          build_id: "b",
          arch: "aarch64",
          relay_url: "https://relay.test",
        });
      if (path.endsWith("/hosts")) return Response.json(hostsByArch);
      return Response.json([]);
    }),
  );
  render(
    <QueryClientProvider client={client}>
      <Dialog.Root open>
        <CreateFarmForm close={vi.fn()} onPending={vi.fn()} initialHost="amd" />
      </Dialog.Root>
    </QueryClientProvider>,
  );
  const user = userEvent.setup();
  // The amd64 host offers both releases; the arm64-only images do not narrow this one.
  const releases = await screen.findByRole("combobox", {
    name: "OpenStack 릴리스",
  });
  await waitFor(() => expect(releases.textContent).toContain("2026.2"));
  await user.click(releases);
  await user.click(
    await screen.findByRole("option", { name: /OpenStack 2024\.1/ }),
  );
  expect(releases.textContent).toContain("2024.1");
  expect(releases.textContent).not.toContain("호스트 불가");

  // The same release on an arm64 host cannot run, and the form says so.
  await user.click(screen.getByRole("combobox", { name: "실행 호스트" }));
  await user.click(await screen.findByRole("option", { name: "local · 로컬" }));
  await screen.findByText(
    /OpenStack 2024\.1의 이미지는 amd64용만 있어 arm64 호스트\(local\)에서 만들 수 없습니다/,
  );
  expect(requested).toContain("/api/v1/releases?arch=aarch64");
  await user.click(screen.getByRole("combobox", { name: "OpenStack 릴리스" }));
  const blocked = await screen.findByRole("option", {
    name: /OpenStack 2024\.1/,
  });
  expect(blocked.getAttribute("aria-disabled")).toBe("true");
  expect(blocked.textContent).toContain("arm64 호스트 불가");
  const allowed = screen.getByRole("option", { name: /OpenStack 2026\.2/ });
  expect(allowed.getAttribute("aria-disabled")).not.toBe("true");
  client.clear();
});

test("labels releases by their templates and shows an assemble job's steps", () => {
  expect(
    releaseLabel({
      release: "2026.2",
      codename: "Hibiscus",
      local_control: 0,
      local_compute: 0,
      bucket_control: 0,
      bucket_compute: 0,
      installable: true,
    }),
  ).toBe("OpenStack 2026.2 (Hibiscus) · 클린 설치");
  expect(
    releaseLabel({
      release: "2026.2",
      codename: "Hibiscus",
      local_control: 2,
      local_compute: 1,
      bucket_control: 0,
      bucket_compute: 1,
      installable: true,
    }),
  ).toBe("OpenStack 2026.2 (Hibiscus) · 템플릿 control 2 · compute 2 · 버킷 1");
  // An end-of-life series shows its code name, capitalized, and has no clean install where
  // its images cannot run.
  expect(
    releaseLabel({
      release: "2021.1",
      series: "wallaby",
      codename: "Wallaby",
      local_control: 0,
      local_compute: 0,
      bucket_control: 0,
      bucket_compute: 0,
      installable: false,
      eol: true,
    }),
  ).toBe("OpenStack 2021.1 (Wallaby) · EOL");
  // A series with no published code name shows its number alone.
  expect(
    releaseLabel({
      release: "2099.1",
      local_control: 0,
      local_compute: 0,
      bucket_control: 0,
      bucket_compute: 0,
      installable: false,
      eol: true,
    }),
  ).toBe("OpenStack 2099.1 · EOL");
  // A release's images name the architectures they run on, and say when the chosen host is
  // not one of them.
  expect(
    releaseLabel(
      {
        release: "2024.1",
        codename: "Caracal",
        local_control: 0,
        local_compute: 0,
        bucket_control: 0,
        bucket_compute: 0,
        installable: true,
        arches: ["x86_64"],
      },
      "aarch64",
    ),
  ).toBe("OpenStack 2024.1 (Caracal) · 클린 설치 · amd64 · arm64 호스트 불가");
  const job = {
    id: "job",
    farm_id: "r",
    kind: "assemble",
    status: "running",
    created_at: 1,
    assembly: [
      {
        action: "restore",
        description:
          "start control on host 'local' from control template 'ctl'",
        status: "succeeded",
        job_id: "restore-job",
      },
      {
        action: "expand",
        description: "add compute-1 on host 'local' as a clean install",
        status: "running",
      },
    ],
  } as Job;
  expect(activityStep({ scope: "farm", job })).toBe(
    "2/2단계 · add compute-1 on host 'local' as a clean install",
  );
  render(<AssemblySteps steps={job.assembly!} />);
  const steps = screen.getByRole("list", { name: "작업 단계" });
  expect(steps.textContent).toContain("1. 완료 · restore-");
  expect(steps.textContent).toContain("2. 진행 중");
});

/** Hosts of the GPU cases: `local` verified the custom kernel, `edge` did not. */
const gpuHosts = (mtty: { local: boolean; edge: boolean }) => [
  {
    id: "local",
    kind: "local",
    address: "localhost",
    ...ready,
    inspection: {
      checked_at: Date.now() / 1000,
      custom_kernel: mtty.local ? "/srv/libkrunfw.5.dylib" : undefined,
      mtty_available: mtty.local,
    },
  },
  {
    id: "edge",
    kind: "ssh",
    address: "10.0.0.21",
    ...ready,
    inspection: {
      checked_at: Date.now() / 1000,
      custom_kernel: mtty.edge ? "/srv/libkrunfw.5.dylib" : undefined,
      mtty_available: mtty.edge,
    },
  },
];

const releases = (...list: string[]) => ({
  arch: "aarch64",
  releases: list.map((release, index) => ({
    release,
    local_control: index === 0 ? 1 : 0,
    local_compute: 0,
    bucket_control: 0,
    bucket_compute: 0,
    installable: true,
  })),
  unreachable: [],
});

/** `/releases/plan` answering with the plan of a farm made from `body`, as the portal shows. */
const releasePlan = (previews: Record<string, unknown>[], offered: string[]) =>
  vi.fn(async (path: string, options?: RequestInit) => {
    if (path.endsWith("/releases")) return Response.json(releases(...offered));
    if (path.endsWith("/releases/plan")) {
      const body = JSON.parse(String(options?.body));
      previews.push(body);
      return Response.json({
        ...plan(
          Array.from(
            { length: body.computes },
            (_, i) => body.compute_hosts[i] || body.host_id,
          ),
        ),
        release: body.release,
      });
    }
    if (path.includes("/catalog")) return Response.json(catalog);
    if (path.endsWith("/plans")) return Response.json(resourcePlan);
    if (path.endsWith("/host"))
      return Response.json({
        build_id: "b",
        relay_url: "https://relay.test",
      });
    if (path.endsWith("/hosts"))
      return Response.json(gpuHosts({ local: true, edge: false }));
    return Response.json([]);
  });

const combo = (name: string) => screen.getByRole("combobox", { name });
const disabledOption = (option: HTMLElement) =>
  option.getAttribute("aria-disabled") === "true" ||
  option.hasAttribute("disabled") ||
  option.dataset.disabled === "true";

test("mtty is offered on every compute's host, not only the control VM's", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  const previews: Record<string, unknown>[] = [];
  vi.stubGlobal("fetch", releasePlan(previews, ["2026.2"]));
  renderForm(client);
  const user = userEvent.setup();
  await user.type(screen.getByPlaceholderText("dev-openstack"), "gpu");
  await user.click(combo("VM 구성"));
  await user.click(
    await screen.findByRole("option", { name: "Control + Compute 2대" }),
  );
  // Both computes run on the host that verified the custom kernel, so mtty is choosable.
  await user.click(combo("GPU 모드"));
  const mtty = await screen.findByRole("option", { name: /mtty/ });
  expect(disabledOption(mtty)).toBe(false);
  await user.click(mtty);
  await waitFor(() =>
    expect(previews.at(-1)).toMatchObject({
      computes: 2,
      gpu: { mode: "mtty", count: 1 },
    }),
  );
  // The second compute on a host without the kernel takes mtty away, says which host, and
  // the request goes out without a GPU instead of being refused.
  await user.click(combo("compute-2 호스트"));
  await user.click(
    await screen.findByRole("option", { name: "compute-2 · edge · 원격" }),
  );
  await waitFor(() => expect(previews.at(-1)).not.toHaveProperty("gpu"));
  expect(screen.getByText(/edge.*mtty 커널 검증을 마치지 않아/)).toBeTruthy();
  expect(combo("GPU 모드").textContent).toContain("없음");
  client.clear();
});

test("lowering the release drops a GPU the farm can no longer have", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  const previews: Record<string, unknown>[] = [];
  vi.stubGlobal("fetch", releasePlan(previews, ["2026.2", "2024.2"]));
  renderForm(client);
  const user = userEvent.setup();
  await user.type(screen.getByPlaceholderText("dev-openstack"), "gpu");
  await user.click(combo("GPU 모드"));
  await user.click(await screen.findByRole("option", { name: /mtty/ }));
  await waitFor(() =>
    expect(previews.at(-1)).toMatchObject({ gpu: { mode: "mtty" } }),
  );
  // 2024.2 has neither the Placement side farm writes nor Nova's mediated-device path, so
  // the farm goes out without a GPU and says why.
  await user.click(combo("OpenStack 릴리스"));
  await user.click(await screen.findByRole("option", { name: /2024\.2/ }));
  await waitFor(() =>
    expect(previews.at(-1)).toMatchObject({ release: "2024.2" }),
  );
  await waitFor(() => expect(previews.at(-1)).not.toHaveProperty("gpu"));
  expect(screen.getByText(/OpenStack 2025\.1 이상에서만/)).toBeTruthy();
  expect(combo("GPU 모드").textContent).toContain("없음");
  client.clear();
});

test("the oldest supported release offers both GPU modes", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  const previews: Record<string, unknown>[] = [];
  vi.stubGlobal("fetch", releasePlan(previews, ["2025.1"]));
  renderForm(client);
  const user = userEvent.setup();
  await user.type(screen.getByPlaceholderText("dev-openstack"), "old");
  await user.click(combo("GPU 모드"));
  // 2025.1's Placement takes a provider with a parent, and farm's Nova backport gives it the
  // mediated-device path, so both modes work on the oldest release farm supports.
  const simulated = await screen.findByRole("option", { name: /simulated/ });
  expect(disabledOption(simulated)).toBe(false);
  const mtty = await screen.findByRole("option", { name: /mtty/ });
  expect(disabledOption(mtty)).toBe(false);
  await user.click(simulated);
  await waitFor(() =>
    expect(previews.at(-1)).toMatchObject({
      release: "2025.1",
      gpu: { mode: "simulated", count: 1, traits: ["CUSTOM_FARM_VGPU"] },
    }),
  );
  await user.click(combo("GPU 모드"));
  await user.click(await screen.findByRole("option", { name: /mtty/ }));
  await waitFor(() =>
    expect(previews.at(-1)).toMatchObject({
      release: "2025.1",
      gpu: { mode: "mtty" },
    }),
  );
  client.clear();
});

test("a simulated GPU carries the fake cards and traits the form names", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  const previews: Record<string, unknown>[] = [];
  vi.stubGlobal("fetch", releasePlan(previews, ["2026.2"]));
  renderForm(client);
  const user = userEvent.setup();
  await user.type(screen.getByPlaceholderText("dev-openstack"), "shaped");
  await user.click(combo("GPU 모드"));
  await user.click(await screen.findByRole("option", { name: /simulated/ }));
  await waitFor(() =>
    expect(previews.at(-1)).toMatchObject({
      gpu: { mode: "simulated", count: 1 },
    }),
  );
  // Two virtual GPUs on two fake cards, on a flavor that requires two CUSTOM_ traits.
  await user.clear(screen.getByLabelText("GPU 수량 (compute마다)"));
  await user.type(screen.getByLabelText("GPU 수량 (compute마다)"), "4");
  await user.clear(screen.getByLabelText("가짜 GPU 장치 수 (compute마다)"));
  await user.type(screen.getByLabelText("가짜 GPU 장치 수 (compute마다)"), "2");
  await user.clear(screen.getByLabelText("flavor가 요구할 trait (쉼표 구분)"));
  await user.type(
    screen.getByLabelText("flavor가 요구할 trait (쉼표 구분)"),
    "CUSTOM_MAAS, CUSTOM_SRIOV",
  );
  await waitFor(() =>
    expect(previews.at(-1)).toMatchObject({
      gpu: {
        mode: "simulated",
        count: 4,
        devices: 2,
        traits: ["CUSTOM_MAAS", "CUSTOM_SRIOV"],
      },
    }),
  );
  // A mediated device takes none of the simulated mode's knobs.
  await user.click(combo("GPU 모드"));
  await user.click(await screen.findByRole("option", { name: /mtty/ }));
  await waitFor(() =>
    expect(previews.at(-1)).toMatchObject({
      gpu: { mode: "mtty", count: 4 },
    }),
  );
  client.clear();
});

/** The catalog a release asks for: services of 2025.1 or newer are unavailable before it. */
const servicesFor = (release?: string) => {
  const old = release !== undefined && release < "2025.1";
  const service = (
    id: string,
    extra: Partial<{
      required: boolean;
      depends_on: string[];
      min_release: string | null;
      estimated: boolean;
      footprint_mib: number;
    }> = {},
  ) => ({
    id,
    required: false,
    depends_on: [],
    requires_data_plane: false,
    available: true,
    min_release: null,
    footprint_mib: 0,
    estimated: false,
    reason: null,
    ...extra,
  });
  const gated = (id: string, extra: Parameters<typeof service>[1]) =>
    service(id, {
      min_release: "2025.1",
      estimated: true,
      ...extra,
    });
  return {
    constraints: [],
    components: [
      service("keystone", { required: true }),
      service("glance", { required: true, depends_on: ["keystone"] }),
      service("nova", {
        depends_on: ["keystone", "glance"],
        footprint_mib: 2048,
      }),
      {
        ...gated("heat", { depends_on: ["keystone"], footprint_mib: 512 }),
        available: !old,
        reason: old
          ? `OpenStack ${release} cannot run 'heat'; it needs 2025.1 or newer`
          : null,
      },
      {
        ...gated("magnum", {
          depends_on: ["keystone", "glance", "heat", "nova"],
          footprint_mib: 512,
        }),
        available: !old,
        reason: old
          ? `OpenStack ${release} cannot run 'magnum'; it needs 2025.1 or newer`
          : null,
      },
    ],
  };
};

/** `releasePlan`, with the catalog each release asks for. */
const servicesFetch = (
  previews: Record<string, unknown>[],
  offered: string[],
) => {
  const inner = releasePlan(previews, offered);
  return vi.fn(async (path: string, options?: RequestInit) => {
    if (path.includes("/catalog")) {
      const release = new URL(path, "http://portal").searchParams.get(
        "release",
      );
      return Response.json(servicesFor(release ?? undefined));
    }
    return inner(path, options);
  });
};

const serviceOption = (name: string) =>
  screen.getByText(name, { selector: "strong" }).closest("label")!;
const serviceBox = (name: string) =>
  serviceOption(name).querySelector("[role=checkbox]")!;

test("a service the chosen release cannot run is locked with the release it needs", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  const previews: Record<string, unknown>[] = [];
  vi.stubGlobal("fetch", servicesFetch(previews, ["2025.2", "2024.2"]));
  renderForm(client);
  const user = userEvent.setup();
  // 2025.2 runs Heat, so it can be picked.
  await screen.findByText("Heat", { selector: "strong" });
  expect(serviceBox("Heat").getAttribute("aria-disabled")).not.toBe("true");
  expect(serviceOption("Heat").textContent).not.toContain("2025.1+");
  await user.click(combo("OpenStack 릴리스"));
  await user.click(await screen.findByRole("option", { name: /2024\.2/ }));
  await waitFor(() =>
    expect(serviceOption("Heat").textContent).toContain("2025.1+"),
  );
  expect(serviceBox("Heat").getAttribute("aria-disabled")).toBe("true");
  expect(serviceBox("Heat").getAttribute("aria-checked")).toBe("false");
  expect(serviceBox("Magnum").getAttribute("aria-disabled")).toBe("true");
  client.clear();
});

test("choosing Magnum checks Heat, locks it, and marks the plan's memory as estimated", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  const previews: Record<string, unknown>[] = [];
  vi.stubGlobal("fetch", servicesFetch(previews, ["2025.2"]));
  renderForm(client);
  const user = userEvent.setup();
  await screen.findByText("Magnum", { selector: "strong" });
  expect(screen.queryByText(/추정 포함/)).toBeNull();
  await user.click(serviceBox("Magnum"));
  expect(serviceBox("Magnum").getAttribute("aria-checked")).toBe("true");
  expect(serviceBox("Heat").getAttribute("aria-checked")).toBe("true");
  expect(serviceBox("Heat").getAttribute("aria-disabled")).toBe("true");
  expect(serviceOption("Heat").textContent).toContain("Magnum에 필요");
  expect(await screen.findByText(/추정 포함/)).toBeTruthy();
  client.clear();
});

test("switching to a release that cannot run a selected service unselects it", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  const previews: Record<string, unknown>[] = [];
  vi.stubGlobal("fetch", servicesFetch(previews, ["2025.2", "2024.2"]));
  renderForm(client);
  const user = userEvent.setup();
  await screen.findByText("Magnum", { selector: "strong" });
  await user.click(serviceBox("Magnum"));
  expect(serviceBox("Heat").getAttribute("aria-checked")).toBe("true");
  await user.click(combo("OpenStack 릴리스"));
  await user.click(await screen.findByRole("option", { name: /2024\.2/ }));
  await waitFor(() =>
    expect(serviceBox("Magnum").getAttribute("aria-checked")).toBe("false"),
  );
  expect(serviceBox("Heat").getAttribute("aria-checked")).toBe("false");
  expect(screen.queryByText(/추정 포함/)).toBeNull();
  client.clear();
});

test("the GPU mode select and the GPU count share a label row when a GPU is on", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  const previews: Record<string, unknown>[] = [];
  vi.stubGlobal("fetch", releasePlan(previews, ["2026.2"]));
  renderForm(client);
  const user = userEvent.setup();
  await user.type(screen.getByPlaceholderText("dev-openstack"), "aligned");
  // With no GPU there is one select under its own label, like the other single selects.
  const noneLabel = combo("GPU 모드").closest(".field, div");
  expect(noneLabel?.querySelector(":scope > span")?.textContent).toBe("GPU");
  await user.click(combo("GPU 모드"));
  await user.click(await screen.findByRole("option", { name: /simulated/ }));
  // With a GPU on, the select and the count are two cells of one grid, each a label + control
  // pair, so both labels and both controls start on the same row.
  const select = combo("GPU 모드");
  const countLabel = screen.getByLabelText("GPU 수량 (compute마다)");
  const selectCell = select.parentElement;
  const grid = selectCell?.parentElement;
  expect(selectCell?.classList.contains("field")).toBe(true);
  expect(selectCell?.querySelector(":scope > span")?.textContent).toBe("GPU");
  expect(countLabel.closest(".field")).toBe(countLabel.closest("label"));
  expect(countLabel.closest(".grid")).toBe(grid);
  expect(grid?.classList.contains("grid")).toBe(true);
});
