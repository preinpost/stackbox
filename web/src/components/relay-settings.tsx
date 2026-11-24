import { Dialog } from "@base-ui/react/dialog";
import { Input } from "@base-ui/react/input";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Radio } from "@base-ui/react/radio";
import { RadioGroup } from "@base-ui/react/radio-group";
import {
  Check,
  Cloud,
  LoaderCircle,
  Network,
  ShieldCheck,
  Waypoints,
} from "lucide-react";
import { useState } from "react";
import {
  api,
  keys,
  relayQuery,
  tunnelStage,
  type RelaySettings,
} from "../api/client";
import { Button, Modal } from "./ui";

type Mode = RelaySettings["mode"];

const modes = [
  {
    value: "quick_tunnel",
    title: "Cloudflare 임시 터널",
    hint: "계정·포트 개방 없이 바로 씁니다. 다른 망의 호스트도 이어집니다.",
    icon: Cloud,
    recommended: true,
  },
  {
    value: "private",
    title: "직접 연결",
    hint: "coordinator 사설 CA로 TLS",
    icon: ShieldCheck,
  },
  {
    value: "proxy",
    title: "직접 띄운 프록시",
    hint: "앞단 프록시가 TLS 처리",
    icon: Waypoints,
  },
] as const;

/** The Hosts page's 릴레이 설정 button, showing whether the relay is up, and its dialog. */
export function RelaySettingsDialog() {
  const relay = useQuery(relayQuery());
  const [open, setOpen] = useState(false);
  const state = !relay.data?.settings
    ? "off"
    : relay.data.error || relay.data.tunnel?.error
      ? "error"
      : relay.data.listening &&
          (!relay.data.tunnel || relay.data.tunnel.connected)
        ? "up"
        : "pending";
  return (
    <Dialog.Root open={open} onOpenChange={setOpen}>
      <Dialog.Trigger render={<Button />}>
        <Network size={16} />
        릴레이 설정
        <span
          className={`relay-dot relay-dot-${state}`}
          title={
            {
              off: "꺼짐",
              error: "오류",
              up: "동작 중",
              pending: "연결 중",
            }[state]
          }
        />
      </Dialog.Trigger>
      {open && <RelaySettingsForm />}
    </Dialog.Root>
  );
}

/** Where stackboxes that span hosts join: the coordinator's relay, with its own CA or behind a proxy. */
function RelaySettingsForm() {
  const client = useQueryClient();
  const relay = useQuery(relayQuery());
  const saved = relay.data?.settings;
  const [url, setUrl] = useState<string>();
  const [mode, setMode] = useState<Mode>();
  const [bind, setBind] = useState<string>();
  const current = {
    url: url ?? saved?.url ?? "",
    mode: mode ?? saved?.mode ?? "quick_tunnel",
    bind: bind ?? saved?.bind ?? "",
  };
  const mutation = useMutation({
    mutationFn: api.setRelay,
    onSuccess: async () => {
      setUrl(undefined);
      setMode(undefined);
      setBind(undefined);
      await Promise.all([
        client.invalidateQueries({ queryKey: keys.relay }),
        client.invalidateQueries({ queryKey: keys.host }),
      ]);
    },
  });
  const update = useMutation({
    mutationFn: api.updateCloudflared,
    onSuccess: () => client.invalidateQueries({ queryKey: keys.relay }),
  });
  const locked = !!relay.data?.spanning_stackboxes?.length;
  const quick = current.mode === "quick_tunnel";
  const tunnel = relay.data?.tunnel;
  const stage = tunnelStage(relay.data);
  const checked = tunnel?.checked_at
    ? new Date(tunnel.checked_at * 1000).toLocaleString()
    : null;
  return (
    <Modal
      title="릴레이 설정"
      description={
        relay.data?.listening
          ? `호스트 간 relay · listening ${relay.data.listening}`
          : relay.data?.error
            ? "호스트 간 relay · listen 실패"
            : saved
              ? "호스트 간 relay · 대기"
              : "호스트 간 relay · 꺼짐"
      }
      closeDisabled={mutation.isPending}
    >
      <form
        onSubmit={(event) => {
          event.preventDefault();
          mutation.mutate({
            url: quick ? "" : current.url.trim(),
            mode: current.mode,
            ...(current.bind.trim() ? { bind: current.bind.trim() } : {}),
          });
        }}
      >
        <div className="modal-body space-y-5">
          <p className="field-hint">
            compute VM을 control VM과 다른 호스트에 둘 때, 호스트들이 이 relay로
            이어집니다. 어떤 방식이든 프레임은 종단 간 암호화됩니다.
          </p>
          <div>
            <span className="field-label" id="relay-mode-label">
              연결 방식
            </span>
            <RadioGroup
              value={current.mode}
              onValueChange={(value) => setMode(value as Mode)}
              aria-labelledby="relay-mode-label"
              disabled={mutation.isPending || locked}
              className="relay-modes"
            >
              {modes.map(({ value, title, hint, icon: Icon, ...mode }) => (
                <label
                  key={value}
                  className="relay-mode"
                  data-wide={"recommended" in mode || undefined}
                >
                  <Radio.Root value={value} className="sr-only" />
                  <Icon size={18} className="relay-mode-icon" />
                  <span>
                    <strong>
                      {title}
                      {"recommended" in mode && (
                        <span className="relay-mode-badge">추천</span>
                      )}
                    </strong>
                    <small>{hint}</small>
                  </span>
                </label>
              ))}
            </RadioGroup>
          </div>
          <label className="field">
            <span>Relay URL</span>
            <Input
              value={quick ? (relay.data?.url ?? "") : current.url}
              readOnly={quick}
              onValueChange={setUrl}
              className="input font-mono"
              placeholder={
                quick
                  ? "저장하면 Cloudflare가 주소를 발급합니다"
                  : current.mode === "private"
                    ? "https://192.168.0.10:10011"
                    : "https://relay.example.com"
              }
              disabled={mutation.isPending || locked}
              required={!quick}
            />
            <small>
              {quick
                ? "coordinator가 다시 뜰 때마다 주소가 바뀌고, 바뀐 주소는 스위치에 자동으로 전달됩니다."
                : "다른 호스트가 닿을 수 있는 주소입니다."}
            </small>
          </label>
          <details className="relay-advanced">
            <summary>고급</summary>
            <label className="field mt-3">
              <span>
                Listen 주소 <span className="optional">선택</span>
              </span>
              <Input
                value={current.bind}
                onValueChange={setBind}
                className="input font-mono"
                placeholder={
                  current.mode === "private"
                    ? "0.0.0.0:10011"
                    : "127.0.0.1:10012"
                }
                disabled={mutation.isPending}
              />
              <small>
                {current.mode === "private"
                  ? "다른 호스트에서 이 포트에 닿아야 합니다."
                  : "TLS 없이 이 주소에서 열리고, 앞의 터널·프록시가 TLS를 맡습니다."}
              </small>
            </label>
          </details>
          {locked && (
            <div className="warning-notice">
              여러 호스트에 걸친 stackbox({relay.data?.spanning_stackboxes?.join(", ")}
              )이 있어 연결 방식과 URL은 바꿀 수 없습니다.
            </div>
          )}
          {tunnel && stage !== "ready" && stage !== "failed" && (
            <TunnelProgress stage={stage} quick={quick} />
          )}
          {tunnel && stage === "ready" && (
            <div className="field-hint flex flex-wrap items-center gap-x-3 gap-y-1">
              <span>
                cloudflared {tunnel.version ?? "설치 중"}
                {tunnel.latest &&
                  tunnel.latest !== tunnel.version &&
                  ` (최신 ${tunnel.latest})`}
                {tunnel.unmanaged && " · STACKBOX_CLOUDFLARED"}
              </span>
              <span>{tunnel.connected ? "터널 연결됨" : "터널 연결 중"}</span>
              {checked && <span>확인 {checked}</span>}
              {!tunnel.unmanaged && (
                <button
                  type="button"
                  className="text-link"
                  disabled={update.isPending}
                  onClick={() => update.mutate()}
                >
                  {update.isPending ? "확인 중…" : "지금 업데이트"}
                </button>
              )}
            </div>
          )}
          {tunnel?.error && (
            <div className="warning-notice">{tunnel.error}</div>
          )}
          {update.error && (
            <div className="warning-notice">{update.error.message}</div>
          )}
          {relay.data?.error && (
            <div className="warning-notice">{relay.data.error}</div>
          )}
          {mutation.error && (
            <div className="warning-notice">{mutation.error.message}</div>
          )}
        </div>
        <div className="modal-footer justify-end">
          <div className="flex gap-2">
            {saved && (
              <Button
                disabled={mutation.isPending || locked}
                onClick={() => mutation.mutate(null)}
              >
                릴레이 끄기
              </Button>
            )}
            <Dialog.Close render={<Button disabled={mutation.isPending} />}>
              닫기
            </Dialog.Close>
            <Button
              type="submit"
              variant="primary"
              disabled={mutation.isPending || (!quick && !current.url.trim())}
            >
              {mutation.isPending ? (
                <>
                  <LoaderCircle size={16} className="animate-spin" />
                  저장 중…
                </>
              ) : (
                "저장"
              )}
            </Button>
          </div>
        </div>
      </form>
    </Modal>
  );
}

const stages = [
  { id: "download", label: "cloudflared 내려받고 검증하는 중" },
  { id: "connect", label: "Cloudflare에 터널 연결하는 중" },
  { id: "address", label: "주소 준비 중 (DNS 반영, 약 15초)" },
] as const;

/** Each step of bringing a tunnel up, with the current one spinning. */
function TunnelProgress({
  stage,
  quick,
}: {
  stage: "download" | "connect" | "address";
  quick: boolean;
}) {
  const steps = stages.filter((s) => quick || s.id !== "address");
  const current = steps.findIndex((s) => s.id === stage);
  return (
    <div className="tunnel-progress" role="status" aria-live="polite">
      <ol>
        {steps.map((step, index) => (
          <li
            key={step.id}
            data-state={
              index < current ? "done" : index === current ? "active" : "todo"
            }
          >
            {index < current ? (
              <Check size={16} />
            ) : index === current ? (
              <LoaderCircle size={16} className="animate-spin" />
            ) : (
              <span className="tunnel-progress-dot" />
            )}
            {step.label}
          </li>
        ))}
      </ol>
      <p>
        coordinator에서 진행되므로 이 창을 닫아도 계속됩니다. 처음 한 번은
        내려받느라 조금 걸릴 수 있어요.
      </p>
    </div>
  );
}
