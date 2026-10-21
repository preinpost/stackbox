import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  cleanup,
  render,
  screen,
  within,
  waitFor,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, test, vi } from "vitest";
import type { ComponentProps } from "react";
import { keys, type Job, type HostJob } from "../api/client";
import { JobsMonitor } from "./jobs-monitor";
import { historyRows, elapsed, type Activity } from "./jobs-model";

vi.mock("@tanstack/react-router", () => ({
  Link: ({
    children,
    to,
    params: _params,
    ...props
  }: ComponentProps<"a"> & { to: string; params?: unknown }) => (
    <a href={to} {...props}>
      {children}
    </a>
  ),
}));
const clients: QueryClient[] = [];
afterEach(() => {
  cleanup();
  clients.forEach((c) => c.clear());
  clients.length = 0;
  vi.unstubAllGlobals();
});
const now = Math.floor(Date.now() / 1000);
function job(
  id: string,
  farm_id: string,
  status: Job["status"],
  created_at = now - 60,
): Job {
  return {
    id,
    farm_id,
    status,
    kind: "deploy",
    phase: "deploy",
    created_at,
    started_at: created_at,
    finished_at: status === "running" ? null : created_at + 15,
    error: status === "failed" ? "private failure detail for " + id : null,
  };
}
function setup(jobs: Job[], hosts: HostJob[] = [], logs: unknown[] = []) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: Infinity } },
  });
  clients.push(client);
  client.setQueryData(keys.jobs, jobs);
  client.setQueryData(keys.hostJobs, hosts);
  client.setQueryData(keys.farms, []);
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string) => {
      if (url.endsWith("/logs")) return Response.json(logs);
      if (url.endsWith("/host-jobs")) return Response.json(hosts);
      if (url.endsWith("/jobs")) return Response.json(jobs);
      return Response.json([]);
    }),
  );
  render(
    <QueryClientProvider client={client}>
      <JobsMonitor />
    </QueryClientProvider>,
  );
  return { client, user: userEvent.setup() };
}

test("separates live work, groups history, reveals prior failures and scopes detail logs to the selected job", async () => {
  const { user } = setup(
    [
      job("active", "live-farm", "running"),
      job("latest", "demo", "succeeded"),
      job("failed", "demo", "failed", now - 120),
    ],
    [],
    [
      { id: 1, job_id: "failed", timestamp: now, message: "selected job log" },
      { id: 2, job_id: "latest", timestamp: now, message: "unrelated job log" },
    ],
  );
  expect(
    within(screen.getByRole("region", { name: "진행 중 작업" })).getByText(
      "live-farm",
    ),
  ).toBeTruthy();
  const table = screen.getByRole("table");
  expect(within(table).queryByText("live-farm")).toBeNull();
  expect(within(table).getAllByRole("row")).toHaveLength(2);
  expect(screen.queryByText("private failure detail for failed")).toBeNull();
  await user.click(screen.getByRole("button", { name: "demo 이전 작업 1개" }));
  expect(within(table).getAllByRole("row")).toHaveLength(3);
  await user.click(
    screen.getByRole("button", { name: "demo OpenStack 배포 상세 failed" }),
  );
  const dialog = await screen.findByRole("dialog");
  expect(
    within(dialog).getByText("private failure detail for failed"),
  ).toBeTruthy();
  await waitFor(() =>
    expect(within(dialog).getByText(/selected job log/)).toBeTruthy(),
  );
  expect(within(dialog).queryByText(/unrelated job log/)).toBeNull();
  expect(within(dialog).queryByRole("link", { name: /Farm 상세/ })).toBeNull();
  expect(
    within(dialog).getByText("삭제된 Farm의 작업 기록입니다."),
  ).toBeTruthy();
  await user.keyboard("{Escape}");
  await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
});

test("keeps host work in its own tab and exposes host event details", async () => {
  const { user } = setup(
    [job("farm", "farm-only", "succeeded")],
    [
      {
        id: "host-job",
        host_id: "remote01",
        kind: "check",
        status: "running",
        created_at: now - 30,
        events: [{ at: now - 10, message: "SSH 연결 확인 중" }],
      },
    ],
  );
  expect(screen.queryByText("remote01")).toBeNull();
  await user.click(screen.getByRole("tab", { name: /호스트 작업/ }));
  expect(await screen.findByText("remote01")).toBeTruthy();
  expect(screen.queryByText("farm-only")).toBeNull();
  await user.click(
    screen.getByRole("button", { name: "remote01 진행 중 작업 상세" }),
  );
  const panel = await screen.findByRole("dialog");
  expect(
    within(panel).getByRole("heading", { name: "작업 이벤트" }),
  ).toBeTruthy();
  expect(within(panel).getAllByText("SSH 연결 확인 중").length).toBeGreaterThan(
    0,
  );
});

test("paginates twenty groups, preserves the page during polling, resets on search and shows zero results", async () => {
  const data = Array.from({ length: 25 }, (_, i) =>
    job("id-" + i, "farm-" + String(i).padStart(2, "0"), "succeeded", now - i),
  );
  const { user, client } = setup(data);
  expect(within(screen.getByRole("table")).getAllByRole("row")).toHaveLength(
    21,
  );
  await user.click(screen.getByRole("button", { name: "다음 페이지" }));
  expect(within(screen.getByRole("table")).getAllByRole("row")).toHaveLength(6);
  client.setQueryData(
    keys.jobs,
    data.map((j) => ({ ...j })),
  );
  await waitFor(() => expect(screen.getByText(/2 \/ 2 페이지/)).toBeTruthy());
  await user.type(
    screen.getByRole("textbox", { name: "작업 검색" }),
    "farm-00",
  );
  expect(
    await screen.findByText(/1개 작업 · 1개 Farm · 1 \/ 1 페이지/),
  ).toBeTruthy();
  await user.clear(screen.getByRole("textbox", { name: "작업 검색" }));
  await user.type(screen.getByRole("textbox", { name: "작업 검색" }), "absent");
  expect(await screen.findByText("조건에 맞는 작업이 없습니다")).toBeTruthy();
  await user.click(screen.getByRole("button", { name: "필터 초기화" }));
  expect(await screen.findByRole("table")).toBeTruthy();
});

test("filtering finds older failures after a newer success without losing timestamps or active work", () => {
  const activities: Activity[] = [
    { scope: "farm", job: job("old", "demo", "failed", now - 2 * 86400) },
    { scope: "farm", job: job("new", "demo", "succeeded", now - 60) },
    { scope: "farm", job: job("live", "other", "running") },
  ];
  const all = { search: "", status: "all", kind: "all", days: "all" };
  expect(historyRows(activities, all, true, now)[0].previous).toHaveLength(1);
  expect(
    historyRows(activities, { ...all, status: "failed" }, true, now)[0].activity
      .job.id,
  ).toBe("old");
  expect(
    historyRows(activities, { ...all, status: "failed", days: "1" }, true, now),
  ).toEqual([]);
  expect(
    historyRows(activities, { ...all, search: " OLD " }, true, now)[0].activity
      .job.id,
  ).toBe("old");
  expect(elapsed(activities[0], now)).toBe("15초");
});
