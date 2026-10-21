import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  ArrowDownToLine,
  Check,
  Copy,
  Pause,
  Play,
  Radio,
  Terminal,
} from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { keys, logsQuery, type LogEntry } from "../api/client";
import { Button, ErrorNotice, Loading } from "./ui";

export function mergeLogs(
  existing: LogEntry[],
  incoming: LogEntry[],
): LogEntry[] {
  const merged = new Map(existing.map((entry) => [entry.id, entry]));
  for (const entry of incoming) merged.set(entry.id, entry);
  return [...merged.values()].sort((a, b) => a.id - b.id).slice(-1000);
}
export function LogViewer({ id }: { id: string }) {
  const client = useQueryClient();
  const logs = useQuery(logsQuery(id));
  const [connection, setConnection] = useState<
    "connecting" | "live" | "reconnecting"
  >("connecting");
  const [gap, setGap] = useState(false);
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
    if (!logs.isSuccess) return;
    const tail = client.getQueryData<LogEntry[]>(keys.logs(id)) ?? [];
    const lastId = tail.at(-1)?.id ?? 0;
    const source = new EventSource(
      `/api/v1/farms/${encodeURIComponent(id)}/logs/stream?after=${lastId}`,
    );
    source.onopen = () => setConnection("live");
    source.onerror = () => setConnection("reconnecting");
    source.addEventListener("gap", () => setGap(true));
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
        setConnection("reconnecting");
      }
    });
    return () => source.close();
  }, [id, logs.isSuccess, client]);
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
            <Radio size={12} />
            {connection === "live"
              ? "LIVE"
              : connection === "connecting"
                ? "CONNECTING"
                : "RECONNECTING"}
          </span>
        </div>
        <div className="flex items-center gap-1">
          <Button
            variant="ghost"
            className="log-button"
            onClick={() => setFollow(!follow)}
            aria-pressed={follow}
          >
            {follow ? <Pause size={13} /> : <Play size={13} />}
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
            {copied ? <Check size={14} /> : <Copy size={14} />}
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
          <div className="log-lines">
            {logs.data.map((entry) => (
              <div className="log-entry" key={entry.id}>
                <span className="log-time">
                  {new Date(entry.timestamp * 1000).toLocaleTimeString(
                    "en-GB",
                    { hour12: false },
                  )}
                </span>
                <span className="log-message">
                  {entry.phase && (
                    <span className="log-phase">[{entry.phase}] </span>
                  )}
                  {entry.message}
                </span>
              </div>
            ))}
          </div>
        )}
        <div ref={end} />
      </div>
      <div className="log-footer">
        <span>{logs.data?.length ?? 0} events · 최근 1,000개 보관</span>
        <button onClick={() => setFollow(true)}>
          <ArrowDownToLine size={12} />맨 아래로
        </button>
      </div>
    </section>
  );
}
