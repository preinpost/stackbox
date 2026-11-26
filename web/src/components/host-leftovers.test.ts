import { describe, expect, it } from "vitest";
import type { HostJob } from "../api/client";
import { jobLeftovers } from "./host-leftovers";

const job = (messages: string[]): HostJob =>
  ({
    id: "j",
    host_id: "dev",
    kind: "purge",
    status: "succeeded",
    events: messages.map((message) => ({ at: 1, message })),
    error: null,
    created_at: 1,
    finished_at: 2,
  }) as unknown as HostJob;

describe("jobLeftovers", () => {
  it("is undefined when the purge left nothing behind", () => {
    expect(jobLeftovers(job(["Removed 1 reserved VM(s)", "Host removed"]))).toBeUndefined();
  });

  it("lists each leftover and the by-hand command on an SSH host", () => {
    const leftovers = jobLeftovers(
      job([
        "Removed stackbox's files",
        "Left on the host: msb and libkrunfw (kept on purpose) | the host's files (could not connect for the cleanup: timed out). To remove the directory by hand, run on the host: rm -rf ~/.local/share/stackbox",
      ]),
    );
    expect(leftovers).toEqual({
      items: [
        "msb and libkrunfw (kept on purpose)",
        "the host's files (could not connect for the cleanup: timed out)",
      ],
      byHand: "rm -rf ~/.local/share/stackbox",
    });
  });

  it("omits the by-hand command for a host the coordinator cannot clean over SSH", () => {
    expect(
      jobLeftovers(job(["Left on the host: this machine's msb and ~/.local/share/stackbox (this coordinator's own host; kept on purpose)"])),
    ).toEqual({
      items: [
        "this machine's msb and ~/.local/share/stackbox (this coordinator's own host; kept on purpose)",
      ],
    });
  });

  it("keeps semicolons inside an item", () => {
    expect(
      jobLeftovers(job(["Left on the host: a (x; y) | b"])),
    ).toEqual({ items: ["a (x; y)", "b"] });
  });

  it("reads the last leftover message when several were written", () => {
    expect(jobLeftovers(job(["Left on the host: a", "Left on the host: b"]))).toEqual({ items: ["b"] });
  });
});
