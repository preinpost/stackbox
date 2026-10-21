import { describe, expect, it } from "vitest";
import { mergeLogs } from "./log-viewer";
import type { LogEntry } from "../api/client";
const entry = (id: number, message = `${id}`): LogEntry => ({
  id,
  message,
  timestamp: 1,
  job_id: "test",
  phase: null,
});
describe("SSE replay cache", () => {
  it("deduplicates reconnects while preserving event order and newer content", () => {
    expect(
      mergeLogs([entry(2), entry(1)], [entry(2, "replayed"), entry(3)]),
    ).toEqual([entry(1), entry(2, "replayed"), entry(3)]);
  });
  it("bounds live log retention to the latest 1,000 events", () => {
    const result = mergeLogs(
      Array.from({ length: 1000 }, (_, i) => entry(i + 1)),
      [entry(1001)],
    );
    expect(result).toHaveLength(1000);
    expect(result[0].id).toBe(2);
    expect(result.at(-1)?.id).toBe(1001);
  });
});
