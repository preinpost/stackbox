import { Dialog } from "@base-ui/react/dialog";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, test, vi } from "vitest";
import type { Job, ReleasePlan } from "../api/client";
import { activityStep } from "../pages/jobs-model";
import { AssemblySteps } from "../pages/jobs-monitor";
import { ReleaseFarmForm, releaseLabel } from "./release-create";

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
      if (path.endsWith("/host")) return Response.json({ build_id: "b" });
      if (path.endsWith("/hosts")) return Response.json(hosts);
      return Response.json([]);
    }),
  );
  const close = vi.fn();
  render(
    <QueryClientProvider client={client}>
      <Dialog.Root open>
        <ReleaseFarmForm close={close} onPending={vi.fn()} />
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
    screen.getByRole("combobox", { name: "control 호스트" }).textContent,
  ).toContain("local");
  expect(screen.getByText(/Farm ID를 입력하면/)).toBeTruthy();
  await user.type(screen.getByPlaceholderText("dev-openstack"), "r");
  await screen.findByText("템플릿 ctl");
  expect(
    screen.getByText(/버킷 minio의 snapshots\/cpt.msb → cpt로 가져와 사용/),
  ).toBeTruthy();

  // A second compute on another host is installed clean, and the preview says why.
  await user.click(screen.getByRole("combobox", { name: "Compute VM 수" }));
  await user.click(await screen.findByRole("option", { name: "Compute 2대" }));
  await user.click(screen.getByRole("combobox", { name: "compute-2 호스트" }));
  await user.click(
    await screen.findByRole("option", { name: "compute-2 · edge · 원격" }),
  );
  await screen.findByText("클린 설치");
  expect(screen.getByText(/not copied to host 'edge'/)).toBeTruthy();
  expect(previews.at(-1)).toMatchObject({
    id: "r",
    release: "2026.2",
    host_id: "local",
    computes: 2,
    compute_hosts: ["", "edge"],
  });

  const start = screen.getByRole("button", { name: "이 계획으로 만들기" });
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
  await waitFor(() => expect(navigate).toHaveBeenCalledWith({ to: "/jobs" }));
  expect(close).toHaveBeenCalled();
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
      if (path.endsWith("/host")) return Response.json({ build_id: "b" });
      if (path.endsWith("/hosts")) return Response.json(hosts);
      return Response.json([]);
    }),
  );
  render(
    <QueryClientProvider client={client}>
      <Dialog.Root open>
        <ReleaseFarmForm close={vi.fn()} onPending={vi.fn()} />
      </Dialog.Root>
    </QueryClientProvider>,
  );
  const user = userEvent.setup();
  await screen.findByText(/조회하지 못한 버킷: minio: connection refused/);
  await user.type(screen.getByPlaceholderText("dev-openstack"), "old");
  await screen.findByText(/installs only OpenStack 2026.2 clean/);
  expect(
    (
      screen.getByRole("button", {
        name: "이 계획으로 만들기",
      }) as HTMLButtonElement
    ).disabled,
  ).toBe(true);
  client.clear();
});

test("labels releases by their templates and shows an assemble job's steps", () => {
  expect(
    releaseLabel({
      release: "2026.2",
      local_control: 0,
      local_compute: 0,
      bucket_control: 0,
      bucket_compute: 0,
      installable: true,
    }),
  ).toBe("OpenStack 2026.2 · control 템플릿 0 · compute 템플릿 0 · 클린 설치");
  const job = {
    id: "job",
    farm_id: "r",
    kind: "assemble",
    status: "running",
    created_at: 1,
    steps: [
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
  render(<AssemblySteps steps={job.steps!} />);
  const steps = screen.getByRole("list", { name: "작업 단계" });
  expect(steps.textContent).toContain("1. 완료 · restore-");
  expect(steps.textContent).toContain("2. 진행 중");
});
