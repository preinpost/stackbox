import { expect, test } from "vitest";
import { gpuSamples, defaultGpuLoad } from "./gpu-load";
import type { GpuLoad } from "../api/client";

const load = (pattern: GpuLoad["pattern"]): GpuLoad => ({
  ...defaultGpuLoad,
  pattern,
  min: 10,
  max: 90,
  period_s: 24,
});

// These are the tables src/gpu.rs samples for the same load; the preview and the guest
// must agree or the portal shows a shape the instance never reports.
test("the preview samples the same table as the coordinator", () => {
  expect(gpuSamples(load("constant"))).toEqual(Array(24).fill(50));
  expect(gpuSamples(load("sine"))).toEqual([
    50, 60, 70, 78, 85, 89, 90, 89, 85, 78, 70, 60, 50, 40, 30, 22, 15, 11, 10,
    11, 15, 22, 30, 40,
  ]);
  expect(gpuSamples(load("triangle"))).toEqual([
    10, 17, 23, 30, 37, 43, 50, 57, 63, 70, 77, 83, 90, 83, 77, 70, 63, 57, 50,
    43, 37, 30, 23, 17,
  ]);
  expect(gpuSamples(load("square"))).toEqual([
    90, 90, 90, 90, 90, 90, 90, 90, 90, 90, 90, 90, 10, 10, 10, 10, 10, 10, 10,
    10, 10, 10, 10, 10,
  ]);
  expect(gpuSamples(load("sawtooth"))).toEqual([
    10, 13, 17, 20, 23, 27, 30, 33, 37, 40, 43, 47, 50, 53, 57, 60, 63, 67, 70,
    73, 77, 80, 83, 87,
  ]);
  expect(gpuSamples(load("random"))).toEqual([
    83, 36, 61, 42, 64, 40, 17, 20, 50, 25, 38, 42, 49, 85, 15, 80, 86, 85, 11,
    42, 16, 53, 16, 46,
  ]);
  expect(gpuSamples(load("spike"))).toEqual([
    10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 90, 10, 10, 10, 10, 10, 10,
    10, 10, 10, 10, 10,
  ]);
});

test("a short period has one sample per second and a long one is capped at 24", () => {
  const short = gpuSamples({ ...defaultGpuLoad, period_s: 5 });
  expect(short).toHaveLength(5);
  const long = gpuSamples({ ...defaultGpuLoad, period_s: 3600 });
  expect(long).toHaveLength(24);
  // The guest's own default when no metadata is set: one GPU at 0%.
  expect(
    gpuSamples({ ...defaultGpuLoad, min: 0, max: 0, pattern: "constant" }),
  ).toEqual(Array(24).fill(0));
});
