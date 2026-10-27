import { expect, test } from "vitest";
import { snapshotHost } from "./lib";

test("labels a snapshot with the platform that froze it", () => {
  expect(snapshotHost({ arch: "aarch64", platform: "macos" })).toBe(
    "aarch64 · macOS",
  );
  expect(snapshotHost({ arch: "x86_64", platform: "linux" })).toBe(
    "x86_64 · Linux",
  );
  // Older snapshots have no platform label.
  expect(snapshotHost({ arch: "aarch64" })).toBe("aarch64");
  expect(snapshotHost({ arch: undefined, platform: null })).toBe("unknown");
});
