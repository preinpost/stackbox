import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, test, vi } from "vitest";
import type { Snapshot } from "../api/client";
import { SnapshotCopies } from "./snapshot-copies";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

const snapshot = {
  id: "tpl",
  status: "ready",
  copies: [
    {
      host_id: "edge",
      reference: "stackbox-snap-tpl",
      via: "bucket",
      size_bytes: 2048,
      copied_at: 1,
    },
  ],
} as unknown as Snapshot;

test("lists the hosts that keep a copy and removes one", async () => {
  const calls: [string, string | undefined][] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string, options?: RequestInit) => {
      calls.push([path, options?.method]);
      return Response.json({ ...snapshot, copies: [] });
    }),
  );
  render(
    <QueryClientProvider client={new QueryClient()}>
      <SnapshotCopies snapshot={snapshot} />
    </QueryClientProvider>,
  );
  expect(screen.getByText("edge")).toBeTruthy();
  await userEvent.click(
    screen.getByRole("button", { name: "edge의 tpl 사본 삭제" }),
  );
  await vi.waitFor(() =>
    expect(calls).toContainEqual([
      "/api/v1/snapshots/tpl/copies/edge",
      "DELETE",
    ]),
  );
});

test("shows nothing for a snapshot without copies", () => {
  const { container } = render(
    <QueryClientProvider client={new QueryClient()}>
      <SnapshotCopies snapshot={{ ...snapshot, copies: [] }} />
    </QueryClientProvider>,
  );
  expect(container.textContent).toBe("");
});
