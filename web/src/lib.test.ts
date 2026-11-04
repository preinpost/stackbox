import { expect, test } from "vitest";
import { archiveSummary, snapshotHost } from "./lib";

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

test("summarizes a bucket archive from its metadata file", () => {
  const metadata = {
    schema: 1,
    key: "snapshots/ctl.msb",
    size_bytes: 1024,
    sha256: "00",
    snapshot_id: "ctl",
    release: "2025.1",
    role: "control" as const,
    components: ["keystone", "nova"],
    arch: "aarch64",
    deployed: true,
    created_at: 1,
    exported_at: 2,
  };
  expect(archiveSummary(metadata).title).toBe(
    "OpenStack 2025.1 · Control 템플릿",
  );
  expect(archiveSummary(metadata).detail).toMatch(/· aarch64$/);
  expect(archiveSummary({ ...metadata, role: null, release: null }).title).toBe(
    "릴리스 미상 · farm 스냅샷",
  );
  // Archives exported before metadata files were written.
  expect(archiveSummary(null).title).toBe("메타데이터 없음");
});
