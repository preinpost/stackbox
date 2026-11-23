import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, expect, test, vi } from "vitest";
import type { Snapshot } from "../api/client";
import { TransferProgress, transferSpeed } from "./snapshots";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

const MiB = 1024 * 1024;

function exportingSnapshot(doneBytes: number, totalBytes: number): Snapshot {
  return {
    id: "dev-farm-001-clean",
    status: "exporting",
    arch: "x86_64",
    plan: {
      components: ["keystone"],
      computes: 1,
      nodes: [],
      memory_mib: 12544,
      disk_mib: 100000,
      warnings: [],
    },
    deployed: true,
    source_exposure: { host: "127.0.0.1", bind: "0.0.0.0", port_base: 15000 },
    transfer: {
      bucket: "s3",
      key: "dev-farm-001-clean.msb",
      done_bytes: doneBytes,
      total_bytes: totalBytes,
    },
    created_at: 1,
    updated_at: 2,
  };
}

test("computes transfer speed across the observed sample window", () => {
  expect(transferSpeed([])).toBeNull();
  expect(transferSpeed([{ done: 64 * MiB, at: 1000 }])).toBeNull();
  expect(
    transferSpeed([
      { done: 64 * MiB, at: 1000 },
      { done: 128 * MiB, at: 9000 },
    ]),
  ).toBe(8 * MiB);
});

test("displays upload speed and estimated remaining time once progress advances", () => {
  let now = 10_000;
  vi.spyOn(Date, "now").mockImplementation(() => now);
  const total = 1024 * MiB;
  const { rerender } = render(
    <TransferProgress snapshot={exportingSnapshot(128 * MiB, total)} />,
  );
  // First sample has no speed or ETA yet.
  expect(screen.getByText(/S3에 올리는 중 · 12%/)).toBeTruthy();
  expect(screen.queryByText(/남음/)).toBeNull();

  // 8 seconds later, 64 MiB more uploaded => 8 MiB/s, 832 MiB left => 104s (1분 44초).
  now += 8_000;
  rerender(<TransferProgress snapshot={exportingSnapshot(192 * MiB, total)} />);
  expect(screen.getByText(/8 MiB\/s · 약 1분 44초 남음/)).toBeTruthy();
});
