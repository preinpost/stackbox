import { Dialog } from "@base-ui/react/dialog";
import { Input } from "@base-ui/react/input";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { CopyPlus, Download, Sparkles } from "lucide-react";
import { useState, type ReactNode } from "react";
import {
  api,
  farmsQuery,
  hostQuery,
  hostsQuery,
  keys,
  releasesQuery,
  type PlannedNode,
  type ReleaseCreateRequest,
  type ReleaseTemplates,
} from "../api/client";
import { hostLabel, hostReady } from "../lib";
import { Button, ErrorNotice, Loading, Modal, SelectInput } from "./ui";

const farmId = /^[a-z0-9]([a-z0-9-]{0,46}[a-z0-9])?$/;

/** How a release is offered: its templates here and in buckets, and whether it installs clean. */
export function releaseLabel(r: ReleaseTemplates) {
  const control = r.local_control + r.bucket_control;
  const compute = r.local_compute + r.bucket_compute;
  const parts = [
    `control 템플릿 ${control}`,
    `compute 템플릿 ${compute}`,
    ...(r.bucket_control + r.bucket_compute
      ? [`버킷 ${r.bucket_control + r.bucket_compute}`]
      : []),
    ...(control === 0 && r.installable ? ["클린 설치"] : []),
  ];
  return `OpenStack ${r.release} · ${parts.join(" · ")}`;
}

/** One VM of the preview: where it comes from and why. */
export function sourceLabel(node: PlannedNode) {
  switch (node.source) {
    case "template":
      return `템플릿 ${node.snapshot_id}`;
    case "bucket":
      return `버킷 ${node.bucket}의 ${node.key} → ${node.snapshot_id}로 가져와 사용`;
    default:
      return "클린 설치";
  }
}

/** Create a farm by choosing an OpenStack release first: the control VM comes from the
 * release's control template, and each compute from a compute template when one boots on
 * its host, otherwise from a clean install. The preview says which for each VM. */
export function ReleaseFarmForm({
  close,
  onPending,
  initialHost,
  header,
}: {
  close: () => void;
  onPending: (pending: boolean) => void;
  initialHost?: string;
  header?: ReactNode;
}) {
  const client = useQueryClient();
  const navigate = useNavigate();
  const releases = useQuery(releasesQuery());
  const coordinator = useQuery(hostQuery());
  const hosts = useQuery(hostsQuery());
  const farms = useQuery(farmsQuery());
  const buildId = coordinator.data?.build_id;
  const readyHosts = (hosts.data ?? []).filter((h) => hostReady(h, buildId));
  const [name, setName] = useState("");
  const [owner, setOwner] = useState("");
  const [picked, setPicked] = useState<string>();
  // Until the user picks one, the release with the most control templates.
  const offered = releases.data?.releases ?? [];
  const release =
    picked ??
    [...offered].sort(
      (a, b) =>
        b.local_control +
        b.bucket_control -
        (a.local_control + a.bucket_control),
    )[0]?.release;
  const [computes, setComputes] = useState(1);
  const [pickedHost, setHost] = useState(initialHost);
  // Templates are kept on the local host, so it is the default for the control VM.
  const hostId =
    pickedHost ??
    (readyHosts.find((h) => h.kind === "local") ?? readyHosts[0])?.id;
  const [computeHosts, setComputeHosts] = useState<string[]>([]);
  const duplicate = farms.data?.some((f) => f.id === name);
  const body: ReleaseCreateRequest | undefined =
    release && hostId && farmId.test(name) && !duplicate
      ? {
          id: name,
          release,
          host_id: hostId,
          computes,
          compute_hosts: Array.from(
            { length: computes },
            (_, i) => computeHosts[i] ?? "",
          ),
          ...(owner.trim() ? { owner: owner.trim() } : {}),
        }
      : undefined;
  const preview = useQuery({
    queryKey: ["release-plan", body],
    queryFn: ({ signal }) => api.releasePlan(body!, signal),
    enabled: !!body,
    retry: false,
    staleTime: 10_000,
  });
  const mutation = useMutation({
    mutationFn: api.createRelease,
    onMutate: () => onPending(true),
    onSettled: () => onPending(false),
    onSuccess: async () => {
      await Promise.all([
        client.invalidateQueries({ queryKey: keys.farms }),
        client.invalidateQueries({ queryKey: keys.jobs }),
        client.invalidateQueries({ queryKey: keys.snapshots }),
      ]);
      close();
      // The farm appears once its first step starts; the job shows every step.
      await navigate({ to: "/jobs" });
    },
  });
  const plan = preview.data;
  return (
    <Modal
      title="새 farm 만들기"
      description="OpenStack 릴리스를 고르면 저장된 템플릿으로 VM을 만들고, 없는 VM은 클린 설치합니다."
      className="create-modal"
      closeDisabled={mutation.isPending}
    >
      <form
        onSubmit={(event) => {
          event.preventDefault();
          if (body && plan) mutation.mutate(body);
        }}
      >
        <div className="modal-body space-y-6">
          {header}
          {releases.isPending ? (
            <Loading label="릴리스를 불러오는 중" />
          ) : releases.error ? (
            <ErrorNotice
              error={releases.error}
              retry={() => void releases.refetch()}
            />
          ) : (
            <div>
              <span className="field-label">OpenStack 릴리스</span>
              <SelectInput
                value={release ?? ""}
                onChange={setPicked}
                disabled={mutation.isPending}
                label="OpenStack 릴리스"
                items={offered.map((r) => ({
                  value: r.release,
                  label: releaseLabel(r),
                }))}
              />
              <p className="field-hint mt-2">
                로컬 스냅샷과 등록된 버킷의 템플릿을 셉니다 (
                {releases.data?.arch}).
                {!!releases.data?.unreachable?.length &&
                  ` 조회하지 못한 버킷: ${releases.data.unreachable.join(", ")}`}
              </p>
            </div>
          )}
          <div className="grid gap-4 sm:grid-cols-2">
            <label className="field">
              <span>
                Farm ID <span className="text-emerald-600">*</span>
              </span>
              <Input
                autoFocus
                value={name}
                onValueChange={setName}
                className="input font-mono"
                placeholder="dev-openstack"
                required
                maxLength={48}
                pattern={"[a-z0-9]([a-z0-9\\-]{0,46}[a-z0-9])?"}
                disabled={mutation.isPending}
              />
              <small>소문자, 숫자, 하이픈 · 최대 48자</small>
              {duplicate && (
                <small className="text-red-600">
                  이미 사용 중인 Farm ID입니다.
                </small>
              )}
            </label>
            <label className="field">
              <span>
                담당자 <span className="optional">선택</span>
              </span>
              <Input
                value={owner}
                onValueChange={setOwner}
                className="input"
                placeholder="이름 또는 팀"
                maxLength={256}
                disabled={mutation.isPending}
              />
            </label>
          </div>
          <div className="grid gap-3 sm:grid-cols-2">
            <SelectInput
              value={String(computes)}
              onChange={(value) => setComputes(Number(value))}
              disabled={mutation.isPending}
              label="Compute VM 수"
              items={[0, 1, 2, 3].map((n) => ({
                value: String(n),
                label: n ? `Compute ${n}대` : "Compute 없이 control만",
              }))}
            />
            <SelectInput
              value={hostId ?? ""}
              onChange={setHost}
              disabled={mutation.isPending || !readyHosts.length}
              label="control 호스트"
              items={readyHosts.map((h) => ({
                value: h.id,
                label: `control · ${hostLabel(h)}`,
              }))}
            />
            {Array.from({ length: computes }, (_, i) => {
              const node = `compute-${i + 1}`;
              return (
                <SelectInput
                  key={node}
                  value={computeHosts[i] || hostId || ""}
                  onChange={(value) =>
                    setComputeHosts((current) => {
                      const next = [...current];
                      next[i] = value === hostId ? "" : value;
                      return next;
                    })
                  }
                  disabled={mutation.isPending}
                  label={`${node} 호스트`}
                  items={readyHosts.map((h) => ({
                    value: h.id,
                    label: `${node} · ${hostLabel(h)}${h.id === hostId ? " (control과 같은 호스트)" : ""}`,
                  }))}
                />
              );
            })}
          </div>
          <section className="plan-summary" aria-label="생성 계획">
            <span className="eyebrow">ENVIRONMENT PLAN</span>
            {!body ? (
              <p>Farm ID를 입력하면 VM마다 어떻게 만들지 미리 보여 줍니다.</p>
            ) : preview.isPending ? (
              <Loading label="계획을 확인하는 중" />
            ) : preview.error ? (
              <p className="text-red-600">{preview.error.message}</p>
            ) : plan ? (
              <>
                <ul className="space-y-2">
                  {[plan.control, ...plan.computes].map((node) => (
                    <li key={node.node}>
                      {node.source === "clean" ? (
                        <Sparkles size={15} />
                      ) : node.source === "bucket" ? (
                        <Download size={15} />
                      ) : (
                        <CopyPlus size={15} />
                      )}{" "}
                      <strong>{node.node}</strong>
                      <span className="muted"> · {node.host_id}</span>:{" "}
                      <span>{sourceLabel(node)}</span>
                      <br />
                      <small className="muted">{node.reason}</small>
                    </li>
                  ))}
                </ul>
                <p>
                  OpenStack {plan.release} · {plan.components.join(", ")}
                </p>
                {plan.notes?.map((note) => (
                  <p key={note} className="muted">
                    {note}
                  </p>
                ))}
              </>
            ) : null}
          </section>
          {mutation.error && <ErrorNotice error={mutation.error} />}
        </div>
        <div className="modal-footer">
          <span className="text-xs text-stone-500">
            단계마다 작업이 하나씩 실행되며, 작업 화면에서 진행을 볼 수
            있습니다.
          </span>
          <div className="flex gap-2">
            <Dialog.Close render={<Button disabled={mutation.isPending} />}>
              취소
            </Dialog.Close>
            <Button
              type="submit"
              variant="primary"
              disabled={!plan || preview.isFetching || mutation.isPending}
            >
              {mutation.isPending ? "시작하는 중" : "이 계획으로 만들기"}
            </Button>
          </div>
        </div>
      </form>
    </Modal>
  );
}
