import {
  useQuery,
  useQueryClient,
  type QueryClient,
} from "@tanstack/react-query";
import {
  ArrowDownToLine,
  Check,
  Copy,
  Pause,
  Play,
  Radio,
  RefreshCw,
  Terminal,
} from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { keys, logsQuery, type LogEntry } from "../api/client";
import { Button, ErrorNotice, Loading } from "./ui";

/** One log line: the time in a fixed column, then the phase and the message. */
export function LogLines({ entries }: { entries: LogEntry[] }) {
  return (
    <div className="log-lines">
      {entries.map((entry) => (
        <div className="log-entry" key={entry.id}>
          <span className="log-time">
            {new Date(entry.timestamp * 1000).toLocaleTimeString("en-GB", {
              hour12: false,
            })}
          </span>
          <span className="log-message">
            {entry.phase && <span className="log-phase">[{entry.phase}] </span>}
            {entry.message}
          </span>
        </div>
      ))}
    </div>
  );
}
export function mergeLogs(
  existing: LogEntry[],
  incoming: LogEntry[],
): LogEntry[] {
  const merged = new Map(existing.map((entry) => [entry.id, entry]));
  for (const entry of incoming) merged.set(entry.id, entry);
  return [...merged.values()].sort((a, b) => a.id - b.id).slice(-1000);
}

/** "closed": the browser gave up retrying, so only an explicit reconnect reopens the stream. */
type StreamConnection = "connecting" | "live" | "reconnecting" | "closed";
type LogStreamState = { connection: StreamConnection; gap: boolean };

/** The panels showing one stackbox's logs share its one EventSource, so a tab switch never doubles it. */
interface LogStream {
  source: EventSource;
  state: LogStreamState;
  listeners: Set<(state: LogStreamState) => void>;
}
const logStreams = new Map<string, LogStream>();

function setStreamState(stream: LogStream, patch: Partial<LogStreamState>) {
  stream.state = { ...stream.state, ...patch };
  for (const listener of stream.listeners) listener(stream.state);
}

function openLogStream(id: string, client: QueryClient): LogStream {
  const stream: LogStream = {
    source: null as unknown as EventSource,
    state: { connection: "connecting", gap: false },
    listeners: new Set(),
  };
  logStreams.set(id, stream);
  attachSource(id, client, stream);
  return stream;
}

/** Opens a new EventSource for the stream, resuming after the last cached event. */
function attachSource(id: string, client: QueryClient, stream: LogStream) {
  const tail = client.getQueryData<LogEntry[]>(keys.logs(id)) ?? [];
  const lastId = tail.at(-1)?.id ?? 0;
  const source = new EventSource(
    `/api/v1/stackboxes/${encodeURIComponent(id)}/logs/stream?after=${lastId}`,
  );
  stream.source = source;
  source.onopen = () => setStreamState(stream, { connection: "live" });
  source.onerror = () =>
    setStreamState(stream, {
      connection:
        source.readyState === EventSource.CLOSED ? "closed" : "reconnecting",
    });
  source.addEventListener("gap", () => setStreamState(stream, { gap: true }));
  source.addEventListener("log", (event) => {
    try {
      const entry = JSON.parse(
        (event as MessageEvent<string>).data,
      ) as LogEntry;
      if (typeof entry.id !== "number" || typeof entry.message !== "string")
        return;
      client.setQueryData<LogEntry[]>(keys.logs(id), (previous = []) =>
        mergeLogs(previous, [entry]),
      );
    } catch {
      setStreamState(stream, { connection: "reconnecting" });
    }
  });
}

/** Replaces a stream the browser gave up on with a fresh connection. */
export function reconnectLogStream(id: string, client: QueryClient) {
  const stream = logStreams.get(id);
  if (!stream) return;
  stream.source.close();
  attachSource(id, client, stream);
  setStreamState(stream, { connection: "connecting", gap: false });
}

export function subscribeLogStream(
  id: string,
  client: QueryClient,
  listener: (state: LogStreamState) => void,
): () => void {
  const stream = logStreams.get(id) ?? openLogStream(id, client);
  stream.listeners.add(listener);
  listener(stream.state);
  return () => {
    stream.listeners.delete(listener);
    if (stream.listeners.size === 0) {
      stream.source.close();
      logStreams.delete(id);
    }
  };
}

/** Follows a stackbox's log stream, opening the one shared connection when `enabled`.
 * The overview tail enables it only while a job runs; the logs tab always does. */
export function useLogStream(
  id: string,
  enabled = true,
): LogStreamState & { reconnect: () => void } {
  const client = useQueryClient();
  const [state, setState] = useState<LogStreamState>({
    connection: "connecting",
    gap: false,
  });
  useEffect(() => {
    if (!enabled) return;
    return subscribeLogStream(id, client, setState);
  }, [id, enabled, client]);
  return { ...state, reconnect: () => reconnectLogStream(id, client) };
}

export function LogViewer({ id }: { id: string }) {
  const logs = useQuery(logsQuery(id));
  const { connection, gap, reconnect } = useLogStream(id, logs.isSuccess);
  const [follow, setFollow] = useState(true);
  const [copied, setCopied] = useState(false);
  const [copyError, setCopyError] = useState("");
  const end = useRef<HTMLDivElement>(null);
  const viewer = useRef<HTMLDivElement>(null);
  const copyTimeout = useRef<ReturnType<typeof setTimeout> | undefined>(
    undefined,
  );
  useEffect(() => () => clearTimeout(copyTimeout.current), []);
  useEffect(() => {
    if (follow && viewer.current)
      viewer.current.scrollTop = viewer.current.scrollHeight;
  }, [logs.data, follow]);
  async function copy() {
    try {
      await navigator.clipboard.writeText(
        logs.data
          ?.map(
            (l) =>
              `[${new Date(l.timestamp * 1000).toLocaleTimeString("en-GB")}] ${l.message.trimEnd()}`,
          )
          .join("\n") ?? "",
      );
      setCopied(true);
      setCopyError("");
      clearTimeout(copyTimeout.current);
      copyTimeout.current = setTimeout(() => setCopied(false), 2000);
    } catch {
      setCopyError("복사 권한이 없습니다. 로그를 선택해 복사하세요.");
    }
  }
  return (
    <section className="log-panel" aria-label="실시간 배포 로그">
      <div className="log-toolbar">
        <div className="flex items-center gap-2">
          <Terminal size={16} />
          <strong>작업 로그</strong>
          <span className={`stream-status ${connection}`}>
            <Radio size={14} />
            {connection === "live"
              ? "LIVE"
              : connection === "connecting"
                ? "CONNECTING"
                : connection === "closed"
                  ? "DISCONNECTED"
                  : "RECONNECTING"}
          </span>
        </div>
        <div className="flex items-center gap-1">
          {connection === "closed" && (
            <Button variant="ghost" className="log-button" onClick={reconnect}>
              <RefreshCw size={15} />
              다시 연결
            </Button>
          )}
          <Button
            variant="ghost"
            className="log-button"
            onClick={() => setFollow(!follow)}
            aria-pressed={follow}
          >
            {follow ? <Pause size={15} /> : <Play size={15} />}
            {follow ? "자동 스크롤" : "스크롤 재개"}
          </Button>
          <Button
            variant="ghost"
            className="log-button"
            aria-label={copied ? "로그 복사 완료" : "로그 복사"}
            onClick={() => {
              void copy();
            }}
          >
            {copied ? <Check size={16} /> : <Copy size={16} />}
          </Button>
        </div>
      </div>
      {gap && (
        <div className="log-note">
          이전 로그의 보관 범위가 지나 최근 1,000개 이벤트부터 표시합니다.
        </div>
      )}
      {copyError && (
        <p role="status" className="log-note">
          {copyError}
        </p>
      )}
      <div
        className="log-body"
        ref={viewer}
        tabIndex={0}
        aria-label="로그 출력"
        onScroll={() => {
          const el = viewer.current;
          if (el && el.scrollHeight - el.scrollTop - el.clientHeight > 80)
            setFollow(false);
        }}
      >
        {logs.isPending ? (
          <Loading label="로그 연결 중" />
        ) : logs.error ? (
          <ErrorNotice
            error={logs.error}
            retry={() => {
              void logs.refetch();
            }}
          />
        ) : !logs.data?.length ? (
          <div className="log-empty">
            <Terminal size={24} />
            <p>아직 기록된 로그가 없습니다.</p>
            <span>작업을 시작하면 출력이 여기에 표시됩니다.</span>
          </div>
        ) : (
          <LogLines entries={logs.data} />
        )}
        <div ref={end} />
      </div>
      <div className="log-footer">
        <span>{logs.data?.length ?? 0} events · 최근 1,000개 보관</span>
        <button onClick={() => setFollow(true)}>
          <ArrowDownToLine size={14} />맨 아래로
        </button>
      </div>
    </section>
  );
}
