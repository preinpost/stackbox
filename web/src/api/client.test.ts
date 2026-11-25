import { afterEach, describe, expect, it, vi } from "vitest";
import { ApiError, api, request } from "./client";
afterEach(() => vi.unstubAllGlobals());
describe("API transport", () => {
  it("sends typed JSON requests without putting a token in the browser", async () => {
    const fetch = vi
      .fn()
      .mockResolvedValue(new Response('{"id":"job"}', { status: 202 }));
    vi.stubGlobal("fetch", fetch);
    expect(
      await request("/stackboxes", { method: "POST", body: '{"id":"demo"}' }),
    ).toEqual({ id: "job" });
    expect(fetch).toHaveBeenCalledWith(
      "/api/v1/stackboxes",
      expect.objectContaining({
        headers: { "Content-Type": "application/json" },
        cache: "no-store",
      }),
    );
  });
  it("preserves actionable backend errors and status codes", async () => {
    vi.stubGlobal(
      "fetch",
      vi
        .fn()
        .mockResolvedValue(
          new Response('{"error":"memory budget exceeded"}', { status: 409 }),
        ),
    );
    await expect(request("/stackboxes")).rejects.toMatchObject({
      status: 409,
      message: "memory budget exceeded",
    });
  });
  it("handles non-JSON proxy errors and malformed success bodies", async () => {
    vi.stubGlobal(
      "fetch",
      vi
        .fn()
        .mockResolvedValueOnce(new Response("Bad gateway", { status: 502 }))
        .mockResolvedValueOnce(new Response("<html/>", { status: 200 })),
    );
    await expect(request("/host")).rejects.toThrow("Bad gateway");
    await expect(request("/host")).rejects.toBeInstanceOf(ApiError);
  });
  it("addCompute sends PCI devices only when some are picked", async () => {
    const fetch = vi.fn(
      async (_url: string, _init?: RequestInit) =>
        new Response('{"id":"job"}', { status: 202 }),
    );
    vi.stubGlobal("fetch", fetch);
    await api.addCompute("grow", "local", undefined, ["0000:04:00.3"]);
    expect(JSON.parse(String(fetch.mock.calls[0][1]?.body))).toEqual({
      host_id: "local",
      pci_devices: ["0000:04:00.3"],
    });
    await api.addCompute("grow", "local", "tpl", []);
    expect(JSON.parse(String(fetch.mock.calls[1][1]?.body))).toEqual({
      host_id: "local",
      from_snapshot: "tpl",
    });
  });
});
