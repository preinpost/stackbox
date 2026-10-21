import { afterEach, describe, expect, it, vi } from "vitest";
import { ApiError, request } from "./client";
afterEach(() => vi.unstubAllGlobals());
describe("API transport", () => {
  it("sends typed JSON requests without putting a token in the browser", async () => {
    const fetch = vi
      .fn()
      .mockResolvedValue(new Response('{"id":"job"}', { status: 202 }));
    vi.stubGlobal("fetch", fetch);
    expect(
      await request("/farms", { method: "POST", body: '{"id":"demo"}' }),
    ).toEqual({ id: "job" });
    expect(fetch).toHaveBeenCalledWith(
      "/api/v1/farms",
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
    await expect(request("/farms")).rejects.toMatchObject({
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
});
