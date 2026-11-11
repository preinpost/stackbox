import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import { Radio, RotateCcw, SquareTerminal } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import type { Farm } from "../api/client";
import { type ShellTarget, shellTargets } from "./shell-targets";
import { Button } from "./ui";

type Connection = "connecting" | "open" | "closed";
/** Text frames from farmd; terminal bytes arrive as binary frames. */
type Control = { type: "exit" } | { type: "error"; message: string };

export function shellUrl(
  location: Location,
  id: string,
  rows: number,
  cols: number,
  node?: string,
) {
  const scheme = location.protocol === "https:" ? "wss:" : "ws:";
  const target = node ? `&node=${encodeURIComponent(node)}` : "";
  return `${scheme}//${location.host}/api/v1/farms/${encodeURIComponent(id)}/shell?rows=${rows}&cols=${cols}${target}`;
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

/**
 * Login shells in the farm's VMs. A multi-node farm picks its VM from a tab bar; each opened
 * VM keeps its session while another is shown. `active` refits the terminal when its tab is shown again.
 */
export function FarmTerminal({
  farm,
  active,
}: {
  farm: Farm;
  active: boolean;
}) {
  const targets = shellTargets(farm.nodes);
  const [selected, setSelected] = useState(
    () =>
      targets.find(({ node }) => node.status === "running")?.key ??
      targets[0]?.key ??
      "",
  );
  const [opened, setOpened] = useState(() => [selected]);
  const select = (key: string) => {
    setSelected(key);
    setOpened((keys) => (keys.includes(key) ? keys : [...keys, key]));
  };
  return (
    <section className="log-panel" aria-label="VM 셸">
      {targets.length > 1 && (
        <div className="terminal-nodes" role="tablist" aria-label="VM 선택">
          {targets.map(({ key, node }) => (
            <button
              key={key}
              type="button"
              role="tab"
              aria-selected={key === selected}
              className={`terminal-node ${key === selected ? "selected" : ""}`}
              disabled={node.status !== "running"}
              title={
                node.status === "running"
                  ? node.name
                  : `${node.name} · ${node.status}`
              }
              onClick={() => select(key)}
            >
              <span
                className={`terminal-node-dot ${node.status === "running" ? "live" : ""}`}
              />
              {key}
            </button>
          ))}
        </div>
      )}
      {opened.map((key) => {
        const target = targets.find((t) => t.key === key);
        return (
          target && (
            <div key={key} hidden={key !== selected}>
              <NodeShell
                farm={farm}
                target={target}
                active={active && key === selected}
              />
            </div>
          )
        );
      })}
    </section>
  );
}

function NodeShell({
  farm,
  target,
  active,
}: {
  farm: Farm;
  target: ShellTarget;
  active: boolean;
}) {
  const container = useRef<HTMLDivElement>(null);
  const fit = useRef<FitAddon | null>(null);
  const [connection, setConnection] = useState<Connection>("connecting");
  const [notice, setNotice] = useState("");
  const [attempt, setAttempt] = useState(0);
  const vm = target.node;
  const node = target.key || undefined;
  useEffect(() => {
    const element = container.current;
    if (!element) return;
    // xterm paints on a canvas, so it needs the theme's colors as values, not variables.
    // The values are read again whenever the page theme swaps them.
    const token = (name: string, fallback: string) =>
      getComputedStyle(element).getPropertyValue(name).trim() || fallback;
    const terminalTheme = () => ({
      background: token("--code-bg", "#f3f5f0"),
      foreground: token("--ink-2", "#454e49"),
      cursor: token("--accent", "#0f7a53"),
      cursorAccent: token("--code-bg", "#f3f5f0"),
      selectionBackground: token("--selection", "#d6ebdf"),
    });
    const terminal = new Terminal({
      cursorBlink: true,
      fontFamily: getComputedStyle(element).fontFamily,
      fontSize: 13,
      lineHeight: 1.2,
      scrollback: 5000,
      theme: terminalTheme(),
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
      shellUrl(window.location, farm.id, terminal.rows, terminal.cols, node),
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
    // A theme switch only rewrites <html data-theme>; repaint the colors without a new session.
    const themeObserver = new MutationObserver(() => {
      terminal.options.theme = terminalTheme();
    });
    themeObserver.observe(document.documentElement, {
      attributes: true,
      attributeFilter: ["data-theme"],
    });
    return () => {
      disposed = true;
      observer.disconnect();
      themeObserver.disconnect();
      subscriptions.forEach((subscription) => subscription.dispose());
      socket.close();
      terminal.dispose();
      fit.current = null;
    };
  }, [farm.id, node, attempt]);
  useEffect(() => {
    if (active && container.current?.offsetWidth) fit.current?.fit();
  }, [active]);
  return (
    <>
      <div className="log-toolbar">
        <div className="flex items-center gap-2">
          <SquareTerminal size={16} />
          <strong>Shell</strong>
          <span className="terminal-vm">{vm.name}</span>
          <span
            className={`stream-status ${connection === "open" ? "live" : connection === "connecting" ? "" : "reconnecting"}`}
          >
            <Radio size={14} />
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
            <RotateCcw size={15} />
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
    </>
  );
}
