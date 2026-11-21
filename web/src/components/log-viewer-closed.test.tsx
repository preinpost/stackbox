import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { keys } from "../api/client";
import { LogViewer } from "./log-viewer";

/** Minimal EventSource stand-in: the test drives error events by hand. */
class FakeEventSource {
  static CONNECTING = 0;
  static OPEN = 1;
  static CLOSED = 2;
  static instances: FakeEventSource[] = [];
  readyState = FakeEventSource.CONNECTING;
  onopen: (() => void) | null = null;
  onerror: (() => void) | null = null;
  constructor(readonly url: string) {
    FakeEventSource.instances.push(this);
  }
  addEventListener() {}
  close() {
    this.readyState = FakeEventSource.CLOSED;
  }
}

describe("LogViewer closed stream", () => {
  beforeEach(() => {
    FakeEventSource.instances = [];
    vi.stubGlobal("EventSource", FakeEventSource);
  });
  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
  });

  it("shows DISCONNECTED with a reconnect button, and reopens the stream on click", async () => {
    const client = new QueryClient();
    client.setQueryData(keys.logs("farm-render"), []);
    render(
      <QueryClientProvider client={client}>
        <LogViewer id="farm-render" />
      </QueryClientProvider>,
    );
    expect(FakeEventSource.instances).toHaveLength(1);
    const first = FakeEventSource.instances[0];

    act(() => {
      first.readyState = FakeEventSource.CLOSED;
      first.onerror?.();
    });

    expect(screen.getByText("DISCONNECTED")).toBeTruthy();
    expect(screen.queryByText("RECONNECTING")).toBeNull();
    const button = screen.getByRole("button", { name: /다시 연결/ });

    await userEvent.click(button);

    expect(FakeEventSource.instances).toHaveLength(2);
    expect(FakeEventSource.instances[1].url).toBe(first.url);
    expect(screen.getByText("CONNECTING")).toBeTruthy();
    expect(screen.queryByRole("button", { name: /다시 연결/ })).toBeNull();
  });
});
