import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import { afterEach, expect, test, vi } from "vitest";
import type { Stackbox, Job } from "../api/client";
import { userEvent } from "@testing-library/user-event";
import type { Datastore } from "../api/client";
import {
  Datastores,
  DatastoreCard,
  DeploymentPhases,
  Endpoints,
  StackboxMeta,
  HorizonLogin,
  NodeTopology,
  PciPassthroughStat,
} from "./stackbox-detail";
import { canFreeze } from "../components/snapshot-actions";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});
const stackbox: Stackbox = {
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

test("names the stackbox's OpenStack release under its name, with a flag outside support", () => {
  const { container, rerender } = render(
    <StackboxMeta
      stackbox={{
        ...stackbox,
        host_id: "dev-machine",
        openstack: {
          release: "2024.1",
          series: "2024.1",
          codename: "Caracal",
          eol: false,
          supported: true,
        },
      }}
    />,
  );
  expect(container.textContent).toContain("원격 · dev-machine");
  // Every release shows its series code name beside the number.
  expect(container.textContent).toContain("OpenStack 2024.1 (Caracal)");
  expect(container.querySelector(".release-flag")).toBeNull();
  // Zed names the images and release notes of 2022.2, which is end of life.
  rerender(
    <StackboxMeta
      stackbox={{
        ...stackbox,
        openstack: {
          release: "2022.2",
          series: "zed",
          codename: "Zed",
          eol: true,
          supported: false,
        },
      }}
    />,
  );
  expect(container.textContent).toContain("OpenStack 2022.2 (Zed)");
  expect(container.querySelector(".release-flag")?.textContent).toBe("EOL");
  // A stackbox made before plans kept their release says so instead of guessing one.
  rerender(<StackboxMeta stackbox={{ ...stackbox, plan: null }} />);
  expect(container.textContent).toContain("릴리스 미기록");
  expect(container.querySelector(".release-flag")).toBeNull();
});

test("shows selected services inside one VM without overlay or separate role nodes", () => {
  const { container } = render(<NodeTopology stackbox={stackbox} />);
  expect(container.querySelectorAll(".topology-node")).toHaveLength(1);
  expect(screen.getByText("stackbox0 10.254.0.2 · 단일 VM")).toBeTruthy();
  // Services bind the fixed service address, not the per-start eth0 address.
  expect(screen.getAllByText("10.254.0.2").length).toBeGreaterThan(0);
  expect(screen.getByText("workspace-all-in-one")).toBeTruthy();
  expect(screen.getByText("Horizon")).toBeTruthy();
  expect(screen.queryByText("Deployer")).toBeNull();
  expect(container.textContent).not.toContain("vx0");
});

test("draws the compute an expansion is adding with its own pending card", () => {
  const multi: Stackbox = {
    id: "local-01",
    status: "deploying",
    host_id: "local",
    created_at: 0,
    updated_at: 0,
    active_job: "job",
    plan: {
      components: ["keystone", "glance", "nova"],
      computes: 1,
      memory_mib: 10240,
      disk_mib: 172032,
      warnings: [],
      nodes: [
        { role: "control", cpus: 4, memory_mib: 7168, disk_mib: 20480 },
        {
          role: "compute",
          index: 1,
          cpus: 2,
          memory_mib: 3072,
          disk_mib: 32768,
        },
      ],
    },
    nodes: [
      {
        name: "local-01-control",
        role: "control",
        memory_mib: 7168,
        status: "running",
      },
      {
        name: "local-01-compute-1",
        role: "compute",
        index: 1,
        memory_mib: 3072,
        status: "running",
      },
    ],
  };
  const job = {
    id: "job",
    kind: "expand",
    target_node: "compute-2",
    target_host: "edge",
  } as Job;
  // Link faults are set through the API, which needs a client.
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  const { container } = render(
    <QueryClientProvider client={client}>
      <NodeTopology stackbox={multi} job={job} />
    </QueryClientProvider>,
  );
  // The committed plan still has two VMs; the third is drawn as on its way.
  expect(container.querySelectorAll(".topology-node")).toHaveLength(3);
  const pending = container.querySelector(".topology-node.pending");
  expect(pending?.textContent).toContain("stackbox-compute-2");
  expect(pending?.textContent).toContain("10.254.0.12");
  expect(pending?.textContent).toContain("생성 중");
  expect(pending?.textContent).toContain("edge");
  // A VM that does not exist yet has no link to fault.
  expect(pending?.querySelector(".node-faults")).toBeNull();
  expect(container.textContent).toContain("VM 3대 · 1대 생성 중");
  client.clear();
});

test("retains observed legacy and unclassified nodes instead of inventing a new layout", () => {
  const { container } = render(
    <NodeTopology
      stackbox={{
        ...stackbox,
        plan: null,
        nodes: [
          {
            name: "old-infra",
            role: "infra",
            memory_mib: 2560,
            status: "running",
          },
          { name: "old-other", role: null, memory_mib: 512, status: "stopped" },
        ],
      }}
    />,
  );
  expect(container.querySelectorAll(".topology-node")).toHaveLength(2);
  expect(screen.getByText("Infrastructure")).toBeTruthy();
  expect(screen.getByText("old-other")).toBeTruthy();
  expect(screen.queryByText("All-in-one")).toBeNull();
  expect(container.textContent).toContain("자동 변환하지 않습니다");
});

test("keeps the step line out of a finished deployment", () => {
  const { container, rerender } = render(
    <DeploymentPhases
      stackbox={{
        ...stackbox,
        status: "deploying",
        active_job: "job-1",
        phase: "verify",
        step: "Cirros 인스턴스 생성과 라우터 경유 ping 검증",
      }}
    />,
  );
  // While the job runs, the fine-grained step stays visible.
  expect(container.querySelectorAll(".phase-step")).toHaveLength(1);
  expect(container.textContent).toContain("Cirros 인스턴스 생성");

  rerender(
    <DeploymentPhases stackbox={{ ...stackbox, status: "ready", phase: "verify" }} />,
  );
  // A ready stackbox shows the finished phases but no leftover step text.
  expect(container.querySelectorAll(".phase-step")).toHaveLength(0);
  expect(container.textContent).not.toContain("Cirros 인스턴스 생성");
  expect(container.querySelectorAll(".phase-done")).toHaveLength(8);
});

const mariadb: Datastore = {
  id: "mariadb",
  label: "MariaDB",
  protocol: "mysql",
  host: "localhost",
  port: 15006,
  username: "root",
  password: "database-secret-value",
  url: "mysql://root@localhost:15006/",
  databases: ["keystone", "nova"],
};

test("keeps the data store password out of the page until it is revealed", async () => {
  const user = userEvent.setup();
  const { container } = render(<DatastoreCard store={mariadb} />);
  expect(screen.getByText("MariaDB")).toBeTruthy();
  // The pill shows the address; the copy button carries the full URL.
  expect(screen.getByText("localhost:15006")).toBeTruthy();
  expect(screen.getByText("keystone")).toBeTruthy();
  expect(container.textContent).not.toContain("database-secret-value");

  await user.click(screen.getByRole("button", { name: "비밀번호 표시" }));
  expect(screen.getByText("database-secret-value")).toBeTruthy();
  await user.click(screen.getByRole("button", { name: "비밀번호 가리기" }));
  expect(container.textContent).not.toContain("database-secret-value");
});

test("offers copy affordances without printing the secret", () => {
  render(<DatastoreCard store={mariadb} />);
  const copy = screen.getByRole("button", { name: "비밀번호 복사" });
  expect(copy.textContent).not.toContain("database-secret-value");
  expect(screen.getByRole("button", { name: "URL 복사" })).toBeTruthy();
});

const login = {
  OS_USERNAME: "admin",
  OS_PASSWORD: "horizon-secret-value",
  OS_USER_DOMAIN_NAME: "Default",
  OS_PROJECT_NAME: "admin",
};

test("hides the Horizon password until asked and offers a copy for every row", async () => {
  const user = userEvent.setup();
  const { container } = render(
    <HorizonLogin
      credentials={login}
      authUrl="http://localhost:15000/v3"
      pending={false}
      error={null}
    />,
  );
  // User name and project both read "admin".
  expect(screen.getAllByText("admin").length).toBeGreaterThanOrEqual(2);
  expect(container.textContent).not.toContain("horizon-secret-value");
  for (const label of ["사용자", "비밀번호", "도메인", "프로젝트"]) {
    expect(screen.getByRole("button", { name: `${label} 복사` })).toBeTruthy();
  }
  expect(
    screen.getByRole("button", { name: "OS_ 환경변수 복사" }),
  ).toBeTruthy();

  await user.click(screen.getByRole("button", { name: "비밀번호 표시" }));
  expect(screen.getByText("horizon-secret-value")).toBeTruthy();
  await user.click(screen.getByRole("button", { name: "비밀번호 가리기" }));
  expect(container.textContent).not.toContain("horizon-secret-value");
  // The copy button never carries the secret as text.
  expect(
    screen.getByRole("button", { name: "비밀번호 복사" }).textContent,
  ).toBe("");
});

test("lays out a multi-node stackbox by VM and cuts a compute off its links", async () => {
  const spec = { cpus: 2, disk_mib: 20480, memory_mib: 3072 };
  const multi: Stackbox = {
    ...stackbox,
    id: "mn",
    status: "ready",
    plan: {
      ...stackbox.plan!,
      components: ["keystone", "glance", "placement", "neutron", "nova"],
      computes: 2,
      nodes: [
        { ...spec, role: "control", cpus: 4, memory_mib: 6656 },
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
  const calls: [string, unknown][] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string, options?: RequestInit) => {
      calls.push([path, JSON.parse(String(options?.body))]);
      return Response.json({ ...multi, faults: { "compute-1": { loss: 1 } } });
    }),
  );
  const client = new QueryClient();
  const { container } = render(
    <QueryClientProvider client={client}>
      <NodeTopology stackbox={multi} />
    </QueryClientProvider>,
  );
  expect(container.querySelectorAll(".topology-node")).toHaveLength(3);
  for (const text of [
    "stackbox-control",
    "stackbox-compute-1",
    "10.254.0.12",
    "mn-compute-2",
  ])
    expect(screen.getByText(text)).toBeTruthy();
  // Nothing to heal yet.
  expect(
    (screen.getAllByRole("button", { name: "복구" })[1] as HTMLButtonElement)
      .disabled,
  ).toBe(true);
  await userEvent.click(screen.getAllByRole("button", { name: "고립" })[1]);
  await waitFor(() => expect(calls).toHaveLength(1));
  expect(calls[0]).toEqual([
    "/api/v1/stackboxes/mn/nodes/compute-1/faults",
    { loss: 1 },
  ]);
  client.clear();
});

test("the public port block covers every port a selected service publishes", () => {
  const exposed: Stackbox = {
    ...stackbox,
    id: "heat-test",
    status: "ready",
    exposure: { bind: "127.0.0.1", host: "127.0.0.1", port_base: 15000 },
    plan: { ...stackbox.plan!, components: ["keystone", "glance", "heat"] },
  };
  const { container } = render(
    <QueryClientProvider client={new QueryClient()}>
      <NodeTopology stackbox={exposed} />
    </QueryClientProvider>,
  );
  // heat publishes its API at 15009 and its CloudFormation API at 15010.
  expect(container.querySelector(".topology-net")?.textContent).toContain(
    "public :15000–15010",
  );
});

test("lists every public endpoint of heat and skyline, not one per component", () => {
  const exposed: Stackbox = {
    ...stackbox,
    id: "heat-test",
    status: "ready",
    exposure: { bind: "127.0.0.1", host: "127.0.0.1", port_base: 15000 },
    plan: {
      ...stackbox.plan!,
      components: ["keystone", "glance", "heat", "skyline"],
    },
  };
  const { container } = render(
    <QueryClientProvider client={new QueryClient()}>
      <Endpoints stackbox={exposed} />
    </QueryClientProvider>,
  );
  expect(container.querySelector(".panel-toolbar .chip")?.textContent).toBe(
    ":15000–15015",
  );
  for (const [name, url] of [
    ["Heat", "http://127.0.0.1:15009"],
    ["Heat CloudFormation", "http://127.0.0.1:15010"],
    ["Skyline", "http://127.0.0.1:15014"],
    ["Skyline Console", "http://127.0.0.1:15015"],
  ] as const) {
    expect(screen.getByText(name, { selector: "strong" })).toBeTruthy();
    expect(screen.getByText(url, { selector: "code" })).toBeTruthy();
  }
});

/** A PCI stackbox names the devices compute-1 holds and cannot be frozen into a snapshot. */
test("names the PCI devices compute-1 holds and refuses a snapshot of that stackbox", () => {
  const pci: Stackbox = {
    ...stackbox,
    host_id: "pci-01",
    exposure: { host: "stackbox.test", bind: "0.0.0.0", port_base: 30000 },
    plan: { ...stackbox.plan!, pci_devices: ["0000:04:00.3", "0000:06:00.0"] },
  };
  const { container } = render(
    <PciPassthroughStat devices={pci.plan!.pci_devices!} />,
  );
  expect(container.textContent).toContain("PCI 패스스루");
  expect(screen.getByText("0000:04:00.3, 0000:06:00.0").className).toContain(
    "font-mono",
  );
  expect(canFreeze({ ...pci, status: "ready" }, ["pci-01"])).toBe(false);
  expect(
    canFreeze(
      {
        ...stackbox,
        host_id: "pci-01",
        exposure: { host: "stackbox.test", bind: "0.0.0.0", port_base: 30000 },
        status: "ready",
      },
      ["pci-01"],
    ),
  ).toBe(true);
});

/** A ready Keystone + Horizon stackbox whose own relay exposure the test drives through the fetch stub. */
const exposureStackbox: Stackbox = {
  ...stackbox,
  id: "stackbox-01",
  status: "ready",
  exposure: { bind: "127.0.0.1", host: "127.0.0.1", port_base: 15000 },
  plan: { ...stackbox.plan!, components: ["keystone", "horizon"] },
};
function renderEndpoints() {
  return render(
    <QueryClientProvider
      client={
        new QueryClient({
          defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
        })
      }
    >
      <Endpoints stackbox={exposureStackbox} />
    </QueryClientProvider>,
  );
}
/** Stubs the endpoints, relay exposure, and hosts calls; the Hosts relay is never read here. */
function stubExposure(initial: Record<string, unknown>) {
  const state = {
    exposure: initial as Record<string, unknown>,
    relay: {
      settings: null,
      url: null,
      tunnel: null,
      bind: null,
      listening: null,
      error: null,
      ca_sha256: null,
      spanning_stackboxes: [],
    } as Record<string, unknown>,
  };
  const bodies: unknown[] = [];
  const calls: string[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string, options?: RequestInit) => {
      const method = options?.method ?? "GET";
      calls.push(`${method} ${path}`);
      if (options?.body) bodies.push(JSON.parse(options.body as string));
      if (path.endsWith("/hosts")) return Response.json([]);
      if (path.endsWith("/relay") && method === "GET")
        return Response.json(state.relay);
      if (path.endsWith("/relay/cloudflared/update") && method === "POST") {
        calls.push("update cloudflared");
        state.relay = {
          ...state.relay,
          tunnel: { ...(state.relay.tunnel as object), version: "2026.11.1" },
        };
        return Response.json(state.relay);
      }
      if (path.endsWith("/endpoints"))
        return Response.json({
          services: {},
          credentials: login,
          datastores: [],
          relay_exposure: state.exposure,
        });
      if (path.endsWith("/relay-exposure") && method === "PUT") {
        const body = JSON.parse(options!.body as string) as { rotate: boolean };
        state.exposure = { enabled: false, state: "starting", urls: {} };
        return Response.json({ ...state.exposure, rotated: body.rotate });
      }
      if (path.endsWith("/relay-exposure") && method === "DELETE") {
        state.exposure = { enabled: false, state: "off", urls: {} };
        return Response.json(state.exposure);
      }
      throw new Error(`unexpected ${method} ${path}`);
    }),
  );
  return { state, bodies, calls };
}

const exposureRelayHost = "https://spine-diego-took-expectations.trycloudflare.com";
const exposureRelayUrls = {
  keystone: `${exposureRelayHost}/x/keystone/v3`,
  horizon: `${exposureRelayHost}/x/horizon/`,
};

test("turns this stackbox's own tunnel on without the Hosts relay, follows starting to ready, then rotates and turns it off", async () => {
  const user = userEvent.setup();
  const writeText = vi.fn(async (_text: string) => {});
  Object.defineProperty(navigator, "clipboard", {
    value: { writeText },
    configurable: true,
  });
  const { state, bodies, calls } = stubExposure({ enabled: false, state: "off", urls: {} });
  renderEndpoints();

  // Hosts → 릴레이 is never consulted, so the off state offers the tunnel directly.
  expect(
    await screen.findByRole("button", { name: "외부 접속 켜기" }),
  ).toBeTruthy();
  expect(
    screen.getByText(
      "이 Stackbox 전용 Cloudflare 터널을 열어 외부에서 OpenStack API·Horizon·DB에 접속합니다.",
    ),
  ).toBeTruthy();
  expect(document.body.textContent).not.toContain("호스트 간 릴레이");
  expect(calls.some((c) => c.endsWith("/api/v1/relay") || c.endsWith("/relay"))).toBe(false);

  // Click: PUT, then the tunnel is still starting.
  await user.click(screen.getByRole("button", { name: "외부 접속 켜기" }));
  expect(calls).toContain("PUT /api/v1/stackboxes/stackbox-01/relay-exposure");
  expect(bodies).toContainEqual({ rotate: false });
  expect(await screen.findByText("Cloudflare 터널 연결 중…")).toBeTruthy();
  expect(screen.getByRole("button", { name: "끄기" })).toBeTruthy();

  // The tunnel publishes its URLs; the endpoints poll (1.5 s) picks them up and the page turns ready.
  state.exposure = {
    enabled: true,
    state: "ready",
    relay_url: exposureRelayHost,
    urls: exposureRelayUrls,
  };
  expect(
    await screen.findByText(exposureRelayUrls.keystone, { selector: "code" }, { timeout: 4000 }),
  ).toBeTruthy();
  expect(screen.getByText(exposureRelayUrls.horizon, { selector: "code" })).toBeTruthy();
  expect(screen.queryByText("Cloudflare 터널 연결 중…")).toBeNull();
  expect(screen.getByRole("button", { name: "외부 터널" }).getAttribute("aria-pressed")).toBe("true");
  expect(screen.getByRole("button", { name: "LAN" })).toBeTruthy();
  expect(screen.getByRole("button", { name: "토큰 재발급" })).toBeTruthy();
  expect(screen.getByRole("button", { name: "끄기" })).toBeTruthy();
  expect(
    screen.getByText(
      "이 Stackbox 전용 Cloudflare 주소입니다. 다른 Stackbox 대시보드와 세션이 섞이지 않으며, 끄거나 토큰을 재발급하면 즉시 차단됩니다.",
    ),
  ).toBeTruthy();
  await user.click(screen.getByRole("button", { name: "OS_ 환경변수 복사" }));
  expect(writeText).toHaveBeenLastCalledWith(
    expect.stringContaining(`export OS_AUTH_URL=${exposureRelayUrls.keystone}`),
  );

  // LAN: an explicit choice stays, and the LAN addresses come back.
  await user.click(screen.getByRole("button", { name: "LAN" }));
  expect(
    screen.getByText("http://127.0.0.1:15000/v3", { selector: "code" }),
  ).toBeTruthy();
  await user.click(screen.getByRole("button", { name: "외부 터널" }));

  // Rotate: a new token, sent as rotate: true.
  state.exposure = {
    enabled: true,
    state: "ready",
    relay_url: exposureRelayHost,
    urls: exposureRelayUrls,
  };
  await user.click(screen.getByRole("button", { name: "토큰 재발급" }));
  await waitFor(() => expect(bodies).toContainEqual({ rotate: true }));

  // Off: DELETE, and the offer to turn it on comes back.
  await user.click(await screen.findByRole("button", { name: "끄기" }));
  expect(calls).toContain("DELETE /api/v1/stackboxes/stackbox-01/relay-exposure");
  expect(
    await screen.findByRole("button", { name: "외부 접속 켜기" }),
  ).toBeTruthy();
});

test("shows the tunnel's failure with retry and off when the exposure is in error", async () => {
  const user = userEvent.setup();
  const { bodies, calls } = stubExposure({
    enabled: false,
    state: "error",
    error: "cloudflared 프로세스가 종료되었습니다",
    urls: {},
  });
  renderEndpoints();

  expect(
    await screen.findByText("cloudflared 프로세스가 종료되었습니다"),
  ).toBeTruthy();
  expect(screen.getByRole("button", { name: "다시 시도" })).toBeTruthy();
  expect(screen.getByRole("button", { name: "끄기" })).toBeTruthy();
  expect(screen.queryByRole("button", { name: "외부 접속 켜기" })).toBeNull();

  await user.click(screen.getByRole("button", { name: "다시 시도" }));
  expect(calls).toContain("PUT /api/v1/stackboxes/stackbox-01/relay-exposure");
  expect(bodies).toContainEqual({ rotate: false });
  expect(await screen.findByText("Cloudflare 터널 연결 중…")).toBeTruthy();
});

test("falls back to the default error text when the tunnel gives no reason", async () => {
  stubExposure({ enabled: false, state: "error", urls: {} });
  renderEndpoints();
  expect(
    await screen.findByText("Cloudflare 터널을 시작하지 못했습니다."),
  ).toBeTruthy();
});

/** The relay-published data stores the endpoints call returns alongside the LAN ports. */
const dataStoreRelayHost = "https://spine-diego-took-expectations.trycloudflare.com";
const dataStoreRelayUrls = {
  mariadb: `${dataStoreRelayHost}/x/7c1e5a/tcp/mariadb`,
  rabbitmq: `${dataStoreRelayHost}/x/7c1e5a/tcp/rabbitmq`,
  "rabbitmq-management": `${dataStoreRelayHost}/x/7c1e5a/rabbitmq-management`,
  "tunnel-script": `${dataStoreRelayHost}/x/7c1e5a/tcp/client.mjs`,
};
const dataStoreTunnelCommand = `curl -fsSL ${dataStoreRelayUrls["tunnel-script"]} | node --input-type=module`;
const rabbitmq: Datastore = {
  id: "rabbitmq",
  label: "RabbitMQ (AMQP)",
  protocol: "amqp",
  host: "localhost",
  port: 15007,
  username: "openstack",
  password: "rabbit-secret-value",
  url: "amqp://openstack@localhost:15007/",
  vhost: "/",
};
const rabbitmqManagement: Datastore = {
  id: "rabbitmq-management",
  label: "RabbitMQ 관리 UI",
  protocol: "http",
  host: "localhost",
  port: 15008,
  username: "openstack",
  url: "http://localhost:15008/",
};

test("shows Data stores through the relay as a zero-install tunnel bar and reuses the store cards", async () => {
  const user = userEvent.setup();
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string) => {
      if (path.endsWith("/endpoints"))
        return Response.json({
          services: {},
          credentials: login,
          datastores: [mariadb, rabbitmq, rabbitmqManagement],
          relay_exposure: {
            enabled: true,
            state: "ready",
            relay_url: dataStoreRelayHost,
            urls: dataStoreRelayUrls,
          },
        });
      throw new Error(`unexpected ${path}`);
    }),
  );
  const ready: Stackbox = {
    ...stackbox,
    id: "stackbox-01",
    status: "ready",
    exposure: { bind: "127.0.0.1", host: "127.0.0.1", port_base: 15000 },
    plan: { ...stackbox.plan!, components: ["keystone", "horizon"] },
  };
  render(
    <QueryClientProvider
      client={
        new QueryClient({
          defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
        })
      }
    >
      <Datastores stackbox={ready} />
    </QueryClientProvider>,
  );

  // Relay is the default view: one curl | node command bar with a copy button.
  const panel = document.querySelector(".datastore-panel") as HTMLElement;
  expect(
    await screen.findByText(dataStoreTunnelCommand, { selector: "code" }),
  ).toBeTruthy();
  expect(screen.getByRole("button", { name: "터널 명령 복사" })).toBeTruthy();
  expect(screen.getByText(/MYSQL_PORT=13306/)).toBeTruthy();
  expect(screen.getByRole("button", { name: "외부 터널" }).getAttribute("aria-pressed")).toBe("true");

  // MariaDB card: the relay-view address and the root account with its password toggle.
  expect(screen.getByText("127.0.0.1:3306", { selector: "code" })).toBeTruthy();
  expect(screen.getByText("root", { selector: "code" })).toBeTruthy();
  expect(screen.getAllByRole("button", { name: "비밀번호 표시" }).length).toBe(2);
  expect(screen.getByText("keystone")).toBeTruthy();

  // RabbitMQ AMQP card: the AMQP address with the openstack account.
  expect(screen.getByText("127.0.0.1:5672", { selector: "code" })).toBeTruthy();
  expect(screen.getAllByText("openstack", { selector: "code" }).length).toBeGreaterThanOrEqual(2);

  // RabbitMQ management UI card: the relay URL, opening in a new tab.
  const consoleLink = screen.getByRole("link", { name: "RabbitMQ 관리 UI 열기" });
  expect(consoleLink.getAttribute("href")).toBe(dataStoreRelayUrls["rabbitmq-management"]);
  expect(consoleLink.getAttribute("target")).toBe("_blank");
  expect(
    screen.getByText(dataStoreRelayUrls["rabbitmq-management"], { selector: "code" }),
  ).toBeTruthy();
  // The relay view never shows the LAN passwords until revealed.
  expect(panel.textContent).not.toContain("database-secret-value");
  expect(panel.textContent).not.toContain("localhost:15006");

  // LAN: the tunnel bar goes away and the original host:port cards come back.
  await user.click(screen.getByRole("button", { name: "LAN" }));
  expect(screen.getByRole("button", { name: "LAN" }).getAttribute("aria-pressed")).toBe("true");
  expect(screen.getByText("localhost:15006")).toBeTruthy();
  expect(screen.getByText("localhost:15007")).toBeTruthy();
  expect(screen.getByText("localhost:15008")).toBeTruthy();
  expect(panel.textContent).not.toContain("curl -fsSL");
  expect(panel.querySelector(".tunnel-bar")).toBeNull();
});

test("a ready external tunnel shows cloudflared's version and updates it when a newer one is out", async () => {
  const user = userEvent.setup();
  const { state, calls } = stubExposure({
    enabled: true,
    state: "ready",
    relay_url: exposureRelayHost,
    urls: exposureRelayUrls,
  });
  state.relay = {
    ...state.relay,
    tunnel: {
      version: "2026.10.1",
      latest: "2026.11.1",
      checked_at: 1_763_000_000,
      connected: true,
      url: exposureRelayHost,
      error: null,
      unmanaged: false,
    },
  };
  renderEndpoints();

  expect(await screen.findByText("cloudflared 2026.10.1")).toBeTruthy();
  await user.click(screen.getByRole("button", { name: "지금 업데이트" }));
  expect(calls).toContain("update cloudflared");
  expect(await screen.findByText("cloudflared 2026.11.1")).toBeTruthy();
  expect(screen.queryByRole("button", { name: "지금 업데이트" })).toBeNull();
});

test("the off state does not read cloudflared's status", async () => {
  const { calls } = stubExposure({ enabled: false, state: "off", urls: {} });
  renderEndpoints();
  expect(await screen.findByRole("button", { name: "외부 접속 켜기" })).toBeTruthy();
  expect(calls.some((c) => c.endsWith("/relay"))).toBe(false);
  expect(document.body.textContent).not.toContain("cloudflared");
});
