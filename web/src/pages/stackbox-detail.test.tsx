import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import { afterEach, expect, test, vi } from "vitest";
import type { Stackbox, Job } from "../api/client";
import { userEvent } from "@testing-library/user-event";
import type { Datastore } from "../api/client";
import {
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
