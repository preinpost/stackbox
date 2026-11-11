import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen, within } from "@testing-library/react";
import { afterEach, expect, test, vi } from "vitest";
import { RemoteHostsPage } from "./remote-hosts";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

/** A row states either the host's status or that its runner is too old, never both. */
test("a host that needs a new runner shows that note instead of its status badge", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  const artifact = {
    sha256: "a".repeat(64),
    os: "linux",
    arch: "x86_64",
    protocol: 1,
    build_id: "build-current",
  };
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string) => {
      if (path.endsWith("/hosts"))
        return Response.json([
          {
            id: "stale-01",
            kind: "ssh",
            address: "10.0.0.5",
            port: 22,
            status: "ready",
            // No single-VM support: the runner predates it.
            agent: { artifact: { ...artifact, supports_single_vm: false } },
          },
          {
            id: "current-01",
            kind: "ssh",
            address: "10.0.0.6",
            port: 22,
            status: "ready",
            agent: { artifact: { ...artifact, supports_single_vm: true } },
          },
        ]);
      if (path.endsWith("/host")) return Response.json({ build_id: "x" });
      return Response.json([]);
    }),
  );
  render(
    <QueryClientProvider client={client}>
      <RemoteHostsPage />
    </QueryClientProvider>,
  );
  // The detail panel repeats the selected host's id, so read the rows from the list.
  const list = (await screen.findByLabelText("호스트 목록")) as HTMLElement;
  const row = async (id: string) =>
    (await within(list).findByText(id)).closest(
      "button.remote-host-row",
    ) as HTMLElement;
  const stale = await row("stale-01");
  const current = await row("current-01");

  expect(stale.textContent).toContain("실행기 업데이트 필요");
  expect(stale.querySelector(".status-badge")).toBeNull();

  expect(current.textContent).not.toContain("실행기 업데이트 필요");
  expect(current.querySelector(".status-badge")?.textContent).toContain(
    "Ready",
  );
  client.clear();
});
