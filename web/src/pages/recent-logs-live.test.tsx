import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, render, screen, waitFor } from "@testing-library/react";
import type { ReactNode } from "react";
import { afterEach, expect, test, vi } from "vitest";
import { keys, type LogEntry } from "../api/client";
import { LogViewer } from "../components/log-viewer";
import { RecentLogs } from "./farm-detail";

type Listener = (event: MessageEvent<string>) => void;

/** An EventSource stand-in that records every connection the panels open. */
class FakeEventSource {
  static instances: FakeEventSource[] = [];
  readonly url: string;
  onopen: (() => void) | null = null;
  onerror: (() => void) | null = null;
  closed = false;
  private listeners = new Map<string, Set<Listener>>();
  constructor(url: string) {
    this.url = url;
    FakeEventSource.instances.push(this);
  }
  addEventListener(type: string, listener: Listener) {
    const listeners = this.listeners.get(type) ?? new Set<Listener>();
    listeners.add(listener);
    this.listeners.set(type, listeners);
  }
  removeEventListener(type: string, listener: Listener) {
    this.listeners.get(type)?.delete(listener);
  }
  close() {
    this.closed = true;
  }
  emit(type: string, data?: unknown) {
    if (type === "open") return this.onopen?.();
    if (type === "error") return this.onerror?.();
    const event = { data: JSON.stringify(data) } as MessageEvent<string>;
    for (const listener of this.listeners.get(type) ?? []) listener(event);
  }
}

const entry = (id: number, message: string): LogEntry => ({
  id,
  message,
  timestamp: 1_700_000_000 + id,
  job_id: "job-1",
  phase: null,
});

function clientWithLogs(id: string, entries: LogEntry[]) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  client.setQueryData(keys.logs(id), entries);
  return client;
}

function renderWithClient(client: QueryClient, ui: ReactNode) {
  return render(
    <QueryClientProvider client={client}>{ui}</QueryClientProvider>,
  );
}

afterEach(() => {
  cleanup();
  FakeEventSource.instances = [];
  vi.unstubAllGlobals();
});

test("the overview tail follows the stream while the farm has an active job", async () => {
  vi.stubGlobal("EventSource", FakeEventSource);
  const client = clientWithLogs("farm-1", [entry(1, "old line")]);
  renderWithClient(
    client,
    <RecentLogs id="farm-1" live onOpenLogs={() => {}} />,
  );

  expect(screen.getByText("old line")).toBeTruthy();
  await waitFor(() => expect(FakeEventSource.instances).toHaveLength(1));
  expect(FakeEventSource.instances[0].url).toContain(
    "/farms/farm-1/logs/stream?after=1",
  );

  act(() => FakeEventSource.instances[0].emit("log", entry(2, "deploying")));
  await waitFor(() => expect(screen.getByText("deploying")).toBeTruthy());
  // The same cache feeds the Logs tab, so the tail and the full viewer agree.
  expect(client.getQueryData<LogEntry[]>(keys.logs("farm-1"))).toHaveLength(2);
});

test("one stream serves the overview and the logs tab of the same farm", async () => {
  vi.stubGlobal("EventSource", FakeEventSource);
  const client = clientWithLogs("farm-1", [entry(1, "old line")]);
  renderWithClient(
    client,
    <>
      <RecentLogs id="farm-1" live onOpenLogs={() => {}} />
      <LogViewer id="farm-1" />
    </>,
  );

  await waitFor(() => expect(FakeEventSource.instances).toHaveLength(1));
  expect(
    FakeEventSource.instances.filter((source) => !source.closed),
  ).toHaveLength(1);
});

test("the overview opens no stream when the farm has no active job", () => {
  vi.stubGlobal("EventSource", FakeEventSource);
  const client = clientWithLogs("farm-2", [entry(1, "old line")]);
  renderWithClient(
    client,
    <RecentLogs id="farm-2" live={false} onOpenLogs={() => {}} />,
  );

  expect(screen.getByText("old line")).toBeTruthy();
  expect(FakeEventSource.instances).toHaveLength(0);
});
