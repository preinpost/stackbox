import { act, cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { StrictMode } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Stackbox } from "../api/client";

/** xterm stand-in: records lifecycle calls and lets the test fire its events. */
const xterm = vi.hoisted(() => {
  type Listener = (value: never) => void;
  class FakeTerminal {
    static instances: FakeTerminal[] = [];
    rows = 24;
    cols = 80;
    options: Record<string, unknown>;
    disposed = false;
    opened: HTMLElement | null = null;
    listeners: Record<string, Set<Listener>> = {
      data: new Set(),
      binary: new Set(),
      resize: new Set(),
    };
    written: Uint8Array[] = [];
    constructor(options: Record<string, unknown>) {
      this.options = { ...options };
      FakeTerminal.instances.push(this);
    }
    private on(kind: string) {
      return (listener: Listener) => {
        this.listeners[kind].add(listener);
        return { dispose: () => this.listeners[kind].delete(listener) };
      };
    }
    onData = this.on("data");
    onBinary = this.on("binary");
    onResize = this.on("resize");
    loadAddon() {}
    open(element: HTMLElement) {
      this.opened = element;
    }
    focus() {}
    write(data: Uint8Array) {
      this.written.push(data);
    }
    dispose() {
      this.disposed = true;
    }
    listenerCount() {
      return Object.values(this.listeners).reduce((n, s) => n + s.size, 0);
    }
  }
  class FakeFitAddon {
    fit() {}
  }
  return { FakeTerminal, FakeFitAddon };
});

vi.mock("@xterm/xterm", () => ({ Terminal: xterm.FakeTerminal }));
vi.mock("@xterm/addon-fit", () => ({ FitAddon: xterm.FakeFitAddon }));
vi.mock("@xterm/xterm/css/xterm.css", () => ({}));

const { StackboxTerminal } = await import("./stackbox-terminal");
const FakeTerminal = xterm.FakeTerminal;

class FakeWebSocket {
  static CONNECTING = 0;
  static OPEN = 1;
  static CLOSING = 2;
  static CLOSED = 3;
  static instances: FakeWebSocket[] = [];
  readyState = FakeWebSocket.CONNECTING;
  binaryType = "blob";
  onopen: (() => void) | null = null;
  onmessage: ((event: { data: unknown }) => void) | null = null;
  onclose: (() => void) | null = null;
  sent: unknown[] = [];
  closeCalls = 0;
  constructor(readonly url: string) {
    FakeWebSocket.instances.push(this);
  }
  send(data: unknown) {
    this.sent.push(data);
  }
  close() {
    this.closeCalls++;
    this.readyState = FakeWebSocket.CLOSED;
  }
  // Server-side events.
  serverOpen() {
    this.readyState = FakeWebSocket.OPEN;
    this.onopen?.();
  }
  serverMessage(data: unknown) {
    this.onmessage?.({ data });
  }
  serverClose() {
    this.readyState = FakeWebSocket.CLOSED;
    this.onclose?.();
  }
}

class FakeResizeObserver {
  static instances: FakeResizeObserver[] = [];
  observed: Element[] = [];
  disconnected = false;
  constructor(readonly callback: () => void) {
    FakeResizeObserver.instances.push(this);
  }
  observe(element: Element) {
    this.observed.push(element);
  }
  unobserve() {}
  disconnect() {
    this.disconnected = true;
  }
}

const RealMutationObserver = globalThis.MutationObserver;
class TrackedMutationObserver extends RealMutationObserver {
  static instances: TrackedMutationObserver[] = [];
  disconnected = false;
  constructor(callback: MutationCallback) {
    super(callback);
    TrackedMutationObserver.instances.push(this);
  }
  disconnect() {
    this.disconnected = true;
    super.disconnect();
  }
}

const node = (name: string, role: string, index?: number, status = "running") =>
  ({ name, role, index, status, memory_mib: 1 }) as never;
const stackbox = (id: string, nodes = [node(`${id}-vm`, "all-in-one")]) =>
  ({ id, name: id, nodes }) as unknown as Stackbox;

/** Sockets the component has not closed itself: a leak once the component is gone. */
const liveSockets = () =>
  FakeWebSocket.instances.filter((s) => s.readyState !== FakeWebSocket.CLOSED);
const liveTerminals = () => FakeTerminal.instances.filter((t) => !t.disposed);

describe("StackboxTerminal lifecycle", () => {
  beforeEach(() => {
    FakeWebSocket.instances = [];
    FakeTerminal.instances = [];
    FakeResizeObserver.instances = [];
    TrackedMutationObserver.instances = [];
    vi.stubGlobal("WebSocket", FakeWebSocket);
    vi.stubGlobal("ResizeObserver", FakeResizeObserver);
    vi.stubGlobal("MutationObserver", TrackedMutationObserver);
  });
  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
  });

  it("opens one socket and one terminal on mount", () => {
    render(<StackboxTerminal stackbox={stackbox("alpha")} active />);
    expect(FakeWebSocket.instances).toHaveLength(1);
    const [socket] = FakeWebSocket.instances;
    expect(socket.url).toBe(
      `ws://${window.location.host}/api/v1/stackboxes/alpha/shell?rows=24&cols=80`,
    );
    expect(socket.binaryType).toBe("arraybuffer");
    expect(FakeTerminal.instances).toHaveLength(1);
    expect(FakeTerminal.instances[0].opened).toBeInstanceOf(HTMLElement);
    expect(FakeResizeObserver.instances[0].observed).toHaveLength(1);
    expect(screen.getByText("CONNECTING")).toBeTruthy();
    act(() => socket.serverOpen());
    expect(screen.getByText("CONNECTED")).toBeTruthy();
  });

  it("relays terminal input and output while open", () => {
    render(<StackboxTerminal stackbox={stackbox("alpha")} active />);
    const [socket] = FakeWebSocket.instances;
    const [terminal] = FakeTerminal.instances;
    // Input before the socket opens is dropped, not thrown.
    for (const l of terminal.listeners.data) l("x" as never);
    expect(socket.sent).toHaveLength(0);
    act(() => socket.serverOpen());
    for (const l of terminal.listeners.data) l("ls" as never);
    for (const l of terminal.listeners.resize)
      l({ rows: 30, cols: 100 } as never);
    expect(socket.sent).toEqual([
      new TextEncoder().encode("ls"),
      JSON.stringify({ type: "resize", rows: 30, cols: 100 }),
    ]);
    act(() => socket.serverMessage(new Uint8Array([104, 105]).buffer));
    expect(Array.from(terminal.written[0])).toEqual([104, 105]);
  });

  it("closes the socket and releases xterm and observers on unmount", () => {
    const { unmount } = render(<StackboxTerminal stackbox={stackbox("alpha")} active />);
    const [socket] = FakeWebSocket.instances;
    const [terminal] = FakeTerminal.instances;
    act(() => socket.serverOpen());
    expect(terminal.listenerCount()).toBe(3);
    unmount();
    expect(socket.closeCalls).toBe(1);
    expect(terminal.disposed).toBe(true);
    expect(terminal.listenerCount()).toBe(0);
    expect(FakeResizeObserver.instances.every((o) => o.disconnected)).toBe(
      true,
    );
    expect(TrackedMutationObserver.instances.length).toBeGreaterThan(0);
    expect(TrackedMutationObserver.instances.every((o) => o.disconnected)).toBe(
      true,
    );
    // A late close event from the dead socket must not touch unmounted state.
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    socket.serverClose();
    expect(error).not.toHaveBeenCalled();
  });

  it("reconnects to the new stackbox after closing the old socket when the stackbox changes", () => {
    const { rerender } = render(<StackboxTerminal stackbox={stackbox("alpha")} active />);
    const [first] = FakeWebSocket.instances;
    act(() => first.serverOpen());
    rerender(<StackboxTerminal stackbox={stackbox("beta")} active />);
    expect(first.closeCalls).toBe(1);
    expect(FakeTerminal.instances[0].disposed).toBe(true);
    expect(FakeWebSocket.instances).toHaveLength(2);
    expect(FakeWebSocket.instances[1].url).toContain("/stackboxes/beta/shell");
    expect(liveSockets()).toEqual([FakeWebSocket.instances[1]]);
    expect(liveTerminals()).toHaveLength(1);
    // The old socket's close event does not mark the new session closed.
    act(() => first.serverClose());
    expect(screen.getByText("CONNECTING")).toBeTruthy();
    expect(screen.queryByRole("status")).toBeNull();
  });

  it("keeps the socket when an unrelated prop changes", () => {
    const { rerender } = render(
      <StackboxTerminal stackbox={stackbox("alpha")} active={false} />,
    );
    rerender(<StackboxTerminal stackbox={stackbox("alpha")} active />);
    expect(FakeWebSocket.instances).toHaveLength(1);
    expect(FakeWebSocket.instances[0].closeCalls).toBe(0);
  });

  it("opens one socket per node tab and closes all of them on unmount", async () => {
    const multi = stackbox("mn", [
      node("mn-control", "control"),
      node("mn-compute-1", "compute", 1),
    ]);
    const { unmount } = render(<StackboxTerminal stackbox={multi} active />);
    expect(FakeWebSocket.instances).toHaveLength(1);
    expect(FakeWebSocket.instances[0].url).toContain("node=control");
    await userEvent.click(screen.getByRole("tab", { name: /compute-1/ }));
    expect(FakeWebSocket.instances).toHaveLength(2);
    expect(FakeWebSocket.instances[1].url).toContain("node=compute-1");
    // Switching back keeps both sessions.
    await userEvent.click(screen.getByRole("tab", { name: /control/ }));
    expect(FakeWebSocket.instances).toHaveLength(2);
    expect(liveSockets()).toHaveLength(2);
    unmount();
    expect(liveSockets()).toHaveLength(0);
    expect(liveTerminals()).toHaveLength(0);
  });

  it("shows a disconnect notice and a reconnect button when the server drops the socket", async () => {
    render(<StackboxTerminal stackbox={stackbox("alpha")} active />);
    const [socket] = FakeWebSocket.instances;
    act(() => socket.serverOpen());
    act(() => socket.serverClose());
    expect(screen.getByText("CLOSED")).toBeTruthy();
    expect(screen.getByRole("status").textContent).toContain(
      "셸 연결이 끊어졌습니다",
    );
    await userEvent.click(screen.getByRole("button", { name: /다시 연결/ }));
    expect(FakeWebSocket.instances).toHaveLength(2);
    expect(liveSockets()).toEqual([FakeWebSocket.instances[1]]);
    expect(liveTerminals()).toHaveLength(1);
    expect(screen.getByText("CONNECTING")).toBeTruthy();
    expect(screen.queryByRole("status")).toBeNull();
  });

  it("shows the server's error message instead of the generic notice", () => {
    render(<StackboxTerminal stackbox={stackbox("alpha")} active />);
    const [socket] = FakeWebSocket.instances;
    act(() => socket.serverOpen());
    act(() =>
      socket.serverMessage(
        JSON.stringify({ type: "error", message: "VM stopped" }),
      ),
    );
    act(() => socket.serverClose());
    expect(screen.getByText("CLOSED")).toBeTruthy();
    expect(screen.getByRole("status").textContent).toBe("VM stopped");
  });

  it("reports a normal shell exit", () => {
    render(<StackboxTerminal stackbox={stackbox("alpha")} active />);
    const [socket] = FakeWebSocket.instances;
    act(() => socket.serverOpen());
    act(() => socket.serverMessage(JSON.stringify({ type: "exit" })));
    act(() => socket.serverClose());
    expect(screen.getByRole("status").textContent).toBe("셸이 종료되었습니다.");
  });

  it("leaks no socket or terminal across the StrictMode double mount", () => {
    const { unmount } = render(
      <StrictMode>
        <StackboxTerminal stackbox={stackbox("alpha")} active />
      </StrictMode>,
    );
    expect(liveSockets()).toHaveLength(1);
    expect(liveTerminals()).toHaveLength(1);
    const live = liveSockets()[0];
    act(() => live.serverOpen());
    // The discarded first socket's close must not flip the live session's status.
    for (const stale of FakeWebSocket.instances.filter((s) => s !== live))
      act(() => stale.serverClose());
    expect(screen.getByText("CONNECTED")).toBeTruthy();
    expect(screen.queryByRole("status")).toBeNull();
    unmount();
    expect(liveSockets()).toHaveLength(0);
    expect(liveTerminals()).toHaveLength(0);
    expect(FakeResizeObserver.instances.every((o) => o.disconnected)).toBe(
      true,
    );
  });
});
