import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, cleanup } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, test, vi } from "vitest";
import { ImportSnapshotDialog } from "./snapshot-actions";

vi.mock("@tanstack/react-router", () => ({
  useNavigate: () => vi.fn(),
  Link: ({ children }: { children: React.ReactNode }) => <a>{children}</a>,
}));
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

test("lists bucket archives by what their metadata files say", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  let submitted: unknown;
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string, options?: RequestInit) => {
      if (path.endsWith("/buckets"))
        return Response.json([
          { id: "minio", name: "farm", prefix: "snapshots/", region: "auto" },
        ]);
      if (path.endsWith("/buckets/minio/objects"))
        return Response.json([
          {
            key: "snapshots/ctl.msb",
            size_bytes: 2048,
            metadata: {
              schema: 1,
              key: "snapshots/ctl.msb",
              size_bytes: 2048,
              sha256: "00",
              snapshot_id: "ctl",
              release: "2025.1",
              role: "control",
              components: ["keystone"],
              arch: "aarch64",
              deployed: true,
              created_at: 1,
              exported_at: 2,
            },
          },
          { key: "snapshots/old.msb", size_bytes: 1024, metadata: null },
        ]);
      if (options?.method === "POST") {
        submitted = JSON.parse(String(options.body));
        return Response.json({ id: "old", status: "importing" });
      }
      return Response.json([]);
    }),
  );
  render(
    <QueryClientProvider client={client}>
      <ImportSnapshotDialog />
    </QueryClientProvider>,
  );
  await userEvent.click(
    await screen.findByRole("button", { name: /가져오기/ }),
  );
  expect(
    await screen.findByText("OpenStack 2025.1 · Control 템플릿"),
  ).toBeTruthy();
  // An archive from before metadata files stays importable.
  const legacy = screen.getByRole("radio", { name: /snapshots\/old\.msb/ });
  expect(legacy.textContent).toContain("메타데이터 없음");
  await userEvent.click(legacy);
  await userEvent.click(screen.getByRole("button", { name: /가져오기 시작/ }));
  await vi.waitFor(() =>
    expect(submitted).toEqual({ bucket: "minio", key: "snapshots/old.msb" }),
  );
});
