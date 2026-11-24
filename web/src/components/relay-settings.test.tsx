import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, test, vi } from "vitest";
import type { RelayStatus } from "../api/client";
import { RelaySettingsDialog } from "./relay-settings";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

const quickSettings = { url: "", mode: "quick_tunnel", bind: "127.0.0.1:10012" };

/** The coordinator's relay status as a quick tunnel reports it at each step. */
const status = (tunnel: Partial<RelayStatus["tunnel"]>): RelayStatus =>
  ({
    settings: quickSettings,
    url: tunnel?.url ?? null,
    tunnel: {
      version: "2026.11.1",
      latest: "2026.11.1",
      checked_at: 1_763_000_000,
      connected: false,
      url: null,
      error: null,
      unmanaged: false,
      ...tunnel,
    },
    bind: "127.0.0.1:10012",
    listening: "127.0.0.1:10012",
    error: null,
    ca_sha256: null,
    spanning_stackboxes: [],
  }) as RelayStatus;

test("saving a quick tunnel once shows its progress until the address is ready", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  // What the coordinator reports: the tunnel connecting, then connected with its address.
  let current: RelayStatus = {
    settings: null,
    url: null,
    tunnel: null,
    bind: null,
    listening: null,
    error: null,
    ca_sha256: null,
    spanning_stackboxes: [],
  } as RelayStatus;
  let puts = 0;
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string, options?: RequestInit) => {
      if (path.endsWith("/relay") && options?.method === "PUT") {
        puts += 1;
        current = status({});
        return Response.json(current);
      }
      if (path.endsWith("/relay")) return Response.json(current);
      throw new Error(`unexpected request ${path}`);
    }),
  );
  render(
    <QueryClientProvider client={client}>
      <RelaySettingsDialog />
    </QueryClientProvider>,
  );

  await userEvent.click(screen.getByRole("button", { name: /릴레이 설정/ }));
  await userEvent.click(await screen.findByRole("button", { name: "저장" }));

  // The save leaves the tunnel connecting; the progress shows that step.
  expect(
    await screen.findByText("Cloudflare에 터널 연결하는 중", {}, { timeout: 5000 }),
  ).toBeTruthy();

  // Connected, but Cloudflare has not given the address yet.
  current = status({ connected: true });
  expect(
    await screen.findByText(/주소 준비 중/, {}, { timeout: 5000 }),
  ).toBeTruthy();

  // The address arrives: the progress is done and the URL is shown.
  current = status({
    connected: true,
    url: "https://calm-river-1234.trycloudflare.com",
  });
  expect(
    await screen.findByText("터널 연결됨", {}, { timeout: 5000 }),
  ).toBeTruthy();
  expect(screen.queryByRole("status")).toBeNull();
  expect(
    screen.getByDisplayValue("https://calm-river-1234.trycloudflare.com"),
  ).toBeTruthy();

  // Polling the status does not save again.
  await new Promise((resolve) => setTimeout(resolve, 2200));
  expect(puts).toBe(1);
});
