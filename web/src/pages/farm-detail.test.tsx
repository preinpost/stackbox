import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import { afterEach, expect, test, vi } from "vitest";
import type { Farm } from "../api/client";
import { userEvent } from "@testing-library/user-event";
import type { Datastore } from "../api/client";
import {
  DatastoreCard,
  DeploymentPhases,
  HorizonLogin,
  NodeTopology,
} from "./farm-detail";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});
const farm: Farm = {
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

test("shows selected services inside one VM without overlay or separate role nodes", () => {
  const { container } = render(<NodeTopology farm={farm} />);
  expect(container.querySelectorAll(".topology-node")).toHaveLength(1);
  expect(screen.getByText("farm0 10.254.0.2 · 단일 VM")).toBeTruthy();
  // Services bind the fixed service address, not the per-start eth0 address.
  expect(screen.getAllByText("10.254.0.2").length).toBeGreaterThan(0);
  expect(screen.getByText("workspace-all-in-one")).toBeTruthy();
  expect(screen.getByText("Horizon")).toBeTruthy();
  expect(screen.queryByText("Deployer")).toBeNull();
  expect(container.textContent).not.toContain("vx0");
});

test("retains observed legacy and unclassified nodes instead of inventing a new layout", () => {
  const { container } = render(
    <NodeTopology
      farm={{
        ...farm,
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
      farm={{
        ...farm,
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
    <DeploymentPhases farm={{ ...farm, status: "ready", phase: "verify" }} />,
  );
  // A ready farm shows the finished phases but no leftover step text.
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

test("lays out a multi-node farm by VM and cuts a compute off its links", async () => {
  const spec = { cpus: 2, disk_mib: 20480, memory_mib: 3072 };
  const multi: Farm = {
    ...farm,
    id: "mn",
    status: "ready",
    plan: {
      ...farm.plan!,
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
      <NodeTopology farm={multi} />
    </QueryClientProvider>,
  );
  expect(container.querySelectorAll(".topology-node")).toHaveLength(3);
  for (const text of [
    "farm-control",
    "farm-compute-1",
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
    "/api/v1/farms/mn/nodes/compute-1/faults",
    { loss: 1 },
  ]);
  client.clear();
});
