import { cleanup, render } from "@testing-library/react";
import { afterEach, expect, test } from "vitest";
import { HostJobHistory } from "./remote-hosts";
import type { HostJob } from "../api/client";

afterEach(cleanup);

/** The kind label sits in a fixed-width column, so a long label must be cut off and still show in full on hover. */
test("a long job kind label is truncated in its column and its full text is offered as a tooltip", () => {
  const job: HostJob = {
    id: "job-1",
    host_id: "dev-01",
    kind: "install_agent",
    status: "succeeded",
    created_at: 1_700_000_000,
    events: [
      {
        at: 1_700_000_001,
        message:
          "Uploading farm-agent, verifying its SHA256 and checking the protocol",
      },
    ],
  } as HostJob;
  const { container } = render(<HostJobHistory job={job} />);
  const label = container.querySelector("summary .job-kind") as HTMLElement;
  expect(label.textContent).toBe("Farm 실행기 업데이트");
  expect(label.getAttribute("title")).toBe("Farm 실행기 업데이트");
  expect(label.classList.contains("job-kind-label")).toBe(true);
});

/** The time sits in a fixed-width, right-aligned column so the status badge after the message lands at the same x on every row. */
test("the job time is in a fixed-width, tabular-number column", () => {
  const job: HostJob = {
    id: "job-2",
    host_id: "dev-01",
    kind: "prepare",
    status: "succeeded",
    created_at: 1_700_000_000,
    events: [],
  } as HostJob;
  const { container } = render(<HostJobHistory job={job} />);
  const time = container.querySelector("summary > time") as HTMLElement;
  expect(time.classList.contains("hostjob-time")).toBe(true);
});
