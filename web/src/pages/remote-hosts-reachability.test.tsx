import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, test, vi } from "vitest";
import { hostReady } from "../lib";
import { RemoteHostsPage } from "./remote-hosts";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

const artifact = {
  sha256: "a".repeat(64),
  os: "linux",
  arch: "x86_64",
  protocol: 1,
  supports_single_vm: true,
  build_id: "current",
};
const now = () => Math.floor(Date.now() / 1000);
const base = {
  kind: "ssh",
  port: 22,
  status: "ready",
  agent: { artifact },
  fingerprint: "SHA256:test",
  username: "dev",
};
const hosts = [
  {
    ...base,
    id: "dev-01",
    address: "192.0.2.10",
    reachability: {
      reachable: false,
      checked_at: now(),
      last_ok_at: now() - 3600,
      error: "SSH handshake timed out after 15s",
      consecutive_failures: 2,
    },
  },
  {
    ...base,
    id: "stale-01",
    address: "192.0.2.11",
    reachability: {
      reachable: true,
      checked_at: now() - 300,
      last_ok_at: now() - 300,
      consecutive_failures: 0,
    },
  },
  {
    ...base,
    id: "fresh-01",
    address: "192.0.2.12",
    reachability: {
      reachable: true,
      checked_at: now(),
      last_ok_at: now(),
      consecutive_failures: 0,
    },
  },
];

function renderPage() {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string) => {
      if (path.endsWith("/hosts")) return Response.json(hosts);
      if (path.endsWith("/host")) return Response.json({ build_id: "current" });
      return Response.json([]);
    }),
  );
  render(
    <QueryClientProvider client={client}>
      <RemoteHostsPage />
    </QueryClientProvider>,
  );
  return client;
}

test("an unreachable host reads as disconnected, not Ready, and a stale check says so", async () => {
  const client = renderPage();
  const list = (await screen.findByLabelText("호스트 목록")) as HTMLElement;
  const row = async (id: string) =>
    (await within(list).findByText(id)).closest(
      "button.remote-host-row",
    ) as HTMLElement;

  const gone = await row("dev-01");
  expect(gone.textContent).toContain("연결 끊김");
  expect(gone.textContent).not.toContain("Ready");

  const stale = await row("stale-01");
  expect(stale.textContent).toContain("확인 지연");
  expect(stale.textContent).not.toContain("Ready");

  const fresh = await row("fresh-01");
  expect(fresh.textContent).toContain("Ready");
  expect(fresh.textContent).not.toContain("연결 끊김");

  // The detail header repeats the state and says what the failure was.
  await userEvent.setup().click(gone);
  const detail = await screen.findByLabelText("dev-01 호스트 상세");
  expect(within(detail).getByText("연결 끊김")).toBeTruthy();
  expect(within(detail).queryByText("Ready")).toBeNull();
  expect(detail.textContent).toContain("SSH handshake timed out after 15s");
  expect(detail.textContent).toContain("마지막 연결");
  client.clear();
});

test("a host that failed its check cannot take a new stackbox", () => {
  const [gone, , fresh] = hosts;
  expect(hostReady(gone as never, "current")).toBe(false);
  expect(hostReady(fresh as never, "current")).toBe(true);
});
