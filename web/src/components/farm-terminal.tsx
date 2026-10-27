import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import { Radio, RotateCcw, SquareTerminal } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import type { Farm } from "../api/client";
import { Button } from "./ui";

type Connection = "connecting" | "open" | "closed";
/** Text frames from farmd; terminal bytes arrive as binary frames. */
type Control = { type: "exit" } | { type: "error"; message: string };

export function shellUrl(
  location: Location,
  id: string,
  rows: number,
  cols: number,
) {
  const scheme = location.protocol === "https:" ? "wss:" : "ws:";
  return `${scheme}//${location.host}/api/v1/farms/${encodeURIComponent(id)}/shell?rows=${rows}&cols=${cols}`;
}

export function parseControl(data: string): Control | null {
  try {
    const message = JSON.parse(data) as Control;
    if (message.type === "exit") return message;
    if (message.type === "error" && typeof message.message === "string")
      return message;
  } catch {
    // Not a control message.
  }
  return null;
}

/** Login shell in the farm's VM. `active` refits the terminal when its tab is shown again. */
export function FarmTerminal({
  farm,
  active,
}: {
  farm: Farm;
  active: boolean;
}) {
  const container = useRef<HTMLDivElement>(null);
  const fit = useRef<FitAddon | null>(null);
  const [connection, setConnection] = useState<Connection>("connecting");
  const [notice, setNotice] = useState("");
  const [attempt, setAttempt] = useState(0);
  const vm = farm.nodes.find((node) => node.role === "all-in-one");
  useEffect(() => {
    const element = container.current;
    if (!element) return;
    const terminal = new Terminal({
      cursorBlink: true,
      fontFamily: getComputedStyle(element).fontFamily,
      fontSize: 13,
      lineHeight: 1.2,
      scrollback: 5000,
      theme: {
        background: "#17291f",
        foreground: "#c9dccf",
        cursor: "#6caf85",
        cursorAccent: "#17291f",
        selectionBackground: "#3e5945",
      },
    });
    const fitAddon = new FitAddon();
    terminal.loadAddon(fitAddon);
    terminal.open(element);
    fit.current = fitAddon;
    const refit = () => {
      // A hidden tab has no size to fit.
      if (element.offsetWidth) fitAddon.fit();
    };
    refit();
    setConnection("connecting");
    setNotice("");
    const socket = new WebSocket(
      shellUrl(window.location, farm.id, terminal.rows, terminal.cols),
    );
    socket.binaryType = "arraybuffer";
    let ended = false;
    // A remount (StrictMode, retry) must ignore the handlers of its own stale socket,
    // or the closed socket reports a disconnect after the new one is already open.
    let disposed = false;
    const send = (data: string | Uint8Array) => {
      if (socket.readyState === WebSocket.OPEN) socket.send(data);
    };
    socket.onopen = () => {
      if (disposed) return;
      setConnection("open");
      terminal.focus();
    };
    socket.onmessage = (event: MessageEvent<string | ArrayBuffer>) => {
      if (disposed) return;
      if (typeof event.data !== "string") {
        terminal.write(new Uint8Array(event.data));
        return;
      }
      const control = parseControl(event.data);
      if (!control) return;
      ended = true;
      setNotice(
        control.type === "error" ? control.message : "셸이 종료되었습니다.",
      );
    };
    socket.onclose = () => {
      if (disposed) return;
      setConnection("closed");
      if (!ended)
        setNotice(
          "셸 연결이 끊어졌습니다. VM 상태를 확인한 뒤 다시 연결하세요.",
        );
    };
    const encoder = new TextEncoder();
    const subscriptions = [
      terminal.onData((data) => send(encoder.encode(data))),
      terminal.onBinary((data) =>
        send(Uint8Array.from(data, (c) => c.charCodeAt(0))),
      ),
      terminal.onResize(({ rows, cols }) =>
        send(JSON.stringify({ type: "resize", rows, cols })),
      ),
    ];
    const observer = new ResizeObserver(refit);
    observer.observe(element);
    return () => {
      disposed = true;
      observer.disconnect();
      subscriptions.forEach((subscription) => subscription.dispose());
      socket.close();
      terminal.dispose();
      fit.current = null;
    };
  }, [farm.id, attempt]);
  useEffect(() => {
    if (active && container.current?.offsetWidth) fit.current?.fit();
  }, [active]);
  return (
    <section className="log-panel" aria-label="VM 셸">
      <div className="log-toolbar">
        <div className="flex items-center gap-2">
          <SquareTerminal size={16} />
          <strong>Shell</strong>
          {vm && <span className="terminal-vm">{vm.name}</span>}
          <span
            className={`stream-status ${connection === "open" ? "live" : connection === "connecting" ? "" : "reconnecting"}`}
          >
            <Radio size={12} />
            {connection === "open"
              ? "CONNECTED"
              : connection === "connecting"
                ? "CONNECTING"
                : "CLOSED"}
          </span>
        </div>
        {connection === "closed" && (
          <Button
            variant="ghost"
            className="log-button"
            onClick={() => setAttempt((n) => n + 1)}
          >
            <RotateCcw size={13} />
            다시 연결
          </Button>
        )}
      </div>
      {notice && (
        <p role="status" className="log-note">
          {notice}
        </p>
      )}
      <div className="terminal-body" ref={container} />
      <div className="log-footer">
        <span>root 로그인 셸 · 페이지를 떠나면 셸도 종료됩니다</span>
      </div>
    </section>
  );
}
