import { expect, test } from "vitest";
import type { Stackbox, Job } from "./api/client";
import {
  archiveSummary,
  stackboxDeployed,
  stackboxState,
  pendingCompute,
  runningVms,
  snapshotHost,
} from "./lib";

test("shows a stackbox as snapshotting while its snapshot job keeps it ready", () => {
  const stackbox = { id: "f", status: "ready" } as Stackbox;
  const snapshot = { id: "j", kind: "snapshot" } as Job;
  expect(stackboxState(stackbox, snapshot)).toBe("snapshotting");
  // Every other job carries its own stackbox status, and a failed stackbox stays failed.
  expect(stackboxState({ ...stackbox, status: "created" }, snapshot)).toBe(
    "snapshotting",
  );
  expect(stackboxState(stackbox, { id: "j", kind: "deploy" } as Job)).toBe("ready");
  expect(stackboxState(stackbox)).toBe("ready");
  expect(stackboxState({ ...stackbox, status: "failed" }, snapshot)).toBe("failed");
  // A coordinator that reports the state itself needs no help from the job.
  expect(stackboxState({ ...stackbox, status: "snapshotting" })).toBe("snapshotting");
  // A deployment still stands while its VM is captured; a created stackbox does not.
  expect(stackboxDeployed({ ...stackbox, status: "snapshotting" })).toBe(true);
  expect(stackboxDeployed({ ...stackbox, status: "created" })).toBe(false);
});

test("names the compute an expansion is adding before the plan keeps it", () => {
  const stackbox = {
    id: "f",
    status: "deploying",
    host_id: "local",
    plan: { computes: 1, compute: { memory_mib: 6144 } },
  } as Stackbox;
  const expand = (target_node?: string, target_host?: string) =>
    ({ id: "j", kind: "expand", target_node, target_host }) as Job;
  expect(pendingCompute(stackbox, expand("compute-2", "edge"))).toEqual({
    role: "compute",
    index: 2,
    host: "edge",
    memory_mib: 6144,
  });
  // A compute the plan already holds is drawn from the plan, and other jobs add nothing.
  expect(pendingCompute(stackbox, expand("compute-1", "local"))).toBeUndefined();
  expect(
    pendingCompute(stackbox, { id: "j", kind: "deploy" } as Job),
  ).toBeUndefined();
  expect(pendingCompute(stackbox, expand(undefined))).toBeUndefined();
  // Without a chosen size the API plans the default compute VM.
  expect(
    pendingCompute(
      { ...stackbox, plan: { computes: 1 } } as Stackbox,
      expand("compute-3"),
    ),
  ).toMatchObject({ index: 3, host: "local", memory_mib: 3072 });
});

test("counts a stackbox's running VMs, naming the compute an expansion is adding", () => {
  const stackbox = {
    id: "f",
    status: "deploying",
    plan: {
      computes: 1,
      nodes: [{ role: "control" }, { role: "compute", index: 1 }],
    },
    nodes: [
      { role: "control", status: "running" },
      { role: "compute", index: 1, status: "running" },
      { role: "compute", index: 2, status: "running" },
    ],
  } as Stackbox;
  expect(runningVms(stackbox)).toBe("3 / 2 running");
  expect(
    runningVms(stackbox, {
      id: "j",
      kind: "expand",
      target_node: "compute-2",
    } as Job),
  ).toBe("3 / 2 running · 1대 추가 중");
});

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
    "릴리스 미상 · stackbox 스냅샷",
  );
  // Archives exported before metadata files were written.
  expect(archiveSummary(null).title).toBe("메타데이터 없음");
});
