import { QueryClient } from "@tanstack/react-query";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  mergeLogs,
  reconnectLogStream,
  subscribeLogStream,
} from "./log-viewer";
import type { LogEntry } from "../api/client";
const entry = (id: number, message = `${id}`): LogEntry => ({
  id,
  message,
  timestamp: 1,
  job_id: "test",
  phase: null,
});
describe("SSE replay cache", () => {
  it("deduplicates reconnects while preserving event order and newer content", () => {
    expect(
      mergeLogs([entry(2), entry(1)], [entry(2, "replayed"), entry(3)]),
    ).toEqual([entry(1), entry(2, "replayed"), entry(3)]);
  });
  it("bounds live log retention to the latest 1,000 events", () => {
    const result = mergeLogs(
      Array.from({ length: 1000 }, (_, i) => entry(i + 1)),
      [entry(1001)],
    );
    expect(result).toHaveLength(1000);
    expect(result[0].id).toBe(2);
    expect(result.at(-1)?.id).toBe(1001);
  });
});

type Listener = (event: MessageEvent<string>) => void;
/** Minimal EventSource stand-in: tests drive open/error/gap events by hand. */
class FakeEventSource {
  static CONNECTING = 0;
  static OPEN = 1;
  static CLOSED = 2;
  static instances: FakeEventSource[] = [];
  readyState = FakeEventSource.CONNECTING;
  onopen: (() => void) | null = null;
  onerror: (() => void) | null = null;
  private listeners = new Map<string, Set<Listener>>();
  constructor(readonly url: string) {
    FakeEventSource.instances.push(this);
  }
  addEventListener(type: string, listener: Listener) {
    const set = this.listeners.get(type) ?? new Set<Listener>();
    set.add(listener);
    this.listeners.set(type, set);
  }
  close() {
    this.readyState = FakeEventSource.CLOSED;
  }
  fire(type: "open" | "error"): void;
  fire(type: string, data: unknown): void;
  fire(type: string, data?: unknown) {
    if (type === "open") return this.onopen?.();
    if (type === "error") return this.onerror?.();
    const event = { data: JSON.stringify(data) } as MessageEvent<string>;
    for (const listener of this.listeners.get(type) ?? []) listener(event);
  }
}

describe("log stream connection state", () => {
  let client: QueryClient;
  const unsubscribers: Array<() => void> = [];
  beforeEach(() => {
    FakeEventSource.instances = [];
    vi.stubGlobal("EventSource", FakeEventSource);
    client = new QueryClient();
  });
  afterEach(() => {
    for (const off of unsubscribers.splice(0)) off();
    vi.unstubAllGlobals();
  });
  function subscribe(
    id: string,
    listener: (s: { connection: string; gap: boolean }) => void,
  ) {
    unsubscribers.push(subscribeLogStream(id, client, listener));
  }
  const current = () => FakeEventSource.instances.at(-1)!;

  it("shows reconnecting while the browser is still retrying", () => {
    let last = { connection: "", gap: false };
    subscribe("stackbox-a", (s) => (last = s));
    current().readyState = FakeEventSource.CONNECTING;
    current().fire("error");
    expect(last.connection).toBe("reconnecting");
  });

  it("marks the stream closed when EventSource gave up, and reconnects on demand", () => {
    let last = { connection: "", gap: false };
    subscribe("stackbox-b", (s) => (last = s));
    const first = current();
    first.readyState = FakeEventSource.CLOSED;
    first.fire("error");
    expect(last.connection).toBe("closed");
    expect(FakeEventSource.instances).toHaveLength(1);

    reconnectLogStream("stackbox-b", client);
    expect(FakeEventSource.instances).toHaveLength(2);
    expect(current().url).toBe(first.url);
    expect(last.connection).toBe("connecting");
  });

  it("clears a stale gap flag when a new stream is opened", () => {
    let last = { connection: "", gap: false };
    subscribe("stackbox-c", (s) => (last = s));
    current().fire("gap", {});
    expect(last.gap).toBe(true);

    current().readyState = FakeEventSource.CLOSED;
    current().fire("error");
    reconnectLogStream("stackbox-c", client);
    expect(last.gap).toBe(false);
  });
});
