import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, expect, test, vi } from "vitest";
import type { Job } from "../api/client";
import { activityStep } from "../pages/jobs-model";
import { currentStep, StepList, StepProgress } from "./job-steps";

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

const freezing: Job = {
  id: "job-1",
  stackbox_id: "src",
  kind: "snapshot",
  status: "running",
  created_at: 900,
  started_at: 900,
  snapshot_id: "clean",
  steps: [
    {
      step: "stop src-all-in-one",
      index: 1,
      total: 4,
      started_at: 900,
      finished_at: 930,
    },
    {
      step: "capture src-all-in-one",
      index: 2,
      total: 4,
      started_at: 930,
    },
  ],
};

test("shows the running step with i/N and the time spent in it", () => {
  vi.useFakeTimers();
  vi.setSystemTime(1000 * 1000);
  render(<StepProgress job={freezing} />);
  const step = screen.getByLabelText("4단계 중 2단계: capture src-all-in-one");
  expect(step.textContent).toBe("2/4 capture src-all-in-one · 1분 10초");
  expect(step.querySelector(".animate-spin")).not.toBeNull();
});

test("shows nothing once the job ended or before its first step", () => {
  const { container, rerender } = render(
    <StepProgress job={{ ...freezing, status: "succeeded" }} />,
  );
  expect(container.textContent).toBe("");
  rerender(<StepProgress job={{ ...freezing, steps: undefined }} />);
  expect(container.textContent).toBe("");
});

test("names the current step in the jobs monitor", () => {
  expect(activityStep({ scope: "stackbox", job: freezing })).toBe(
    "2/4 capture src-all-in-one",
  );
  // Jobs without numbered steps keep their phase description.
  expect(currentStep({ ...freezing, steps: [] })).toBeUndefined();
});

test("lists every step and marks the one a failed job stopped in", () => {
  const failed: Job = {
    ...freezing,
    status: "failed",
    finished_at: 960,
    steps: [
      freezing.steps![0],
      { ...freezing.steps![1], finished_at: 960, failed: true },
    ],
  };
  render(<StepList job={failed} now={2000} />);
  const items = screen.getByLabelText("작업 단계").querySelectorAll("li");
  expect([...items].map((li) => li.className)).toEqual(["done", "failed"]);
  expect(items[1].textContent).toContain("capture src-all-in-one");
  // A finished step counts to its end, not to now.
  expect(items[1].textContent).toContain("30초");
});
