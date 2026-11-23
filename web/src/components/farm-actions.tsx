import { Dialog } from "@base-ui/react/dialog";
import { Input } from "@base-ui/react/input";
import { Checkbox } from "@base-ui/react/checkbox";
import {
  keepPreviousData,
  useMutation,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import {
  ArrowRight,
  Boxes,
  Check,
  CopyPlus,
  Cpu,
  Download,
  HardDrive,
  LoaderCircle,
  Lock,
  Play,
  Plus,
  RefreshCw,
  ShieldCheck,
  Sparkles,
  Trash2,
  TriangleAlert,
} from "lucide-react";
import { useEffect, useState } from "react";
import {
  api,
  catalogQuery,
  farmsQuery,
  hostQuery,
  hostsQuery,
  keys,
  releasesQuery,
  snapshotsQuery,
  type ComputeSize,
  type Farm,
  type GpuMode,
  type HostPciDevice,
  type Job,
  type Phase,
  type ReleaseCreateRequest,
  type RemoteHost,
  type Snapshot,
} from "../api/client";
import {
  componentInfo,
  hostKernelNote,
  hostLabel,
  hostNotice,
  hostReady,
  remoteAgentNeedsUpdate,
  componentLabel,
  farmComponents,
  awaitsCompute,
  farmNodesRunning,
  isDeployableFarm,
  isMultiNodeFarm,
  memory,
  phases,
  resolveComponents,
  snapshotOrigin,
} from "../lib";
import {
  Button,
  CopyText,
  ErrorNotice,
  ForgetOption,
  Loading,
  Modal,
  SelectInput,
} from "./ui";
import { archName, releaseLabel, runsOn, sourceLabel } from "./release-create";

/** A `year.release` at least `min`; a legacy series name never is. */
function releaseAtLeast(release: string | undefined, min: string) {
  return !!release && /^\d{4}\.\d+$/.test(release) && release >= min;
}

async function refresh(client: ReturnType<typeof useQueryClient>) {
  await Promise.all([
    client.invalidateQueries({ queryKey: keys.farms }),
    client.invalidateQueries({ queryKey: keys.jobs }),
    client.invalidateQueries({ queryKey: keys.host }),
    client.invalidateQueries({ queryKey: keys.hosts }),
  ]);
}
/** Compute templates a new compute of `farm` can boot from: a compute VM of the farm's
 * OpenStack release with the same services. A template of another release is never an
 * option — its containers could not join the farm's control plane — and the API checks the
 * same rule again when the job starts. */
export const computeTemplatesFor = (
  farm: Farm,
  snapshots: Snapshot[] = [],
  arch?: string | null,
) => {
  const release = farm.plan?.release;
  const components = (farm.plan?.components ?? []).join(",");
  return snapshots.filter(
    (s) =>
      s.template === "compute" &&
      ["ready", "exporting"].includes(s.status) &&
      (!arch || s.arch === arch) &&
      s.plan.components.join(",") === components &&
      // A farm recorded before releases were kept takes whatever the coordinator installs.
      (!release || s.release === release),
  );
};
function publicHostName(hostId: string, hosts: RemoteHost[] = []) {
  const chosen = hosts.find((h) => h.id === hostId);
  if (chosen?.kind === "local") return "localhost";
  return chosen && /^[a-z0-9.-]+$/i.test(chosen.address) ? chosen.address : "";
}
/** Compute VM sizes the create form offers. Few disk sizes keep few golden images per host. */
export const farmId = /^[a-z0-9]([a-z0-9-]{0,46}[a-z0-9])?$/;

/** One bucket the release search could not reach, shown as one short line.
 * `detail` keeps the coordinator's own error for the log it folds away. */
export type BucketIssue = {
  bucket: string;
  reason: string;
  detail: string;
};

/** The plan route reports the same failures as notes with this prefix. */
const bucketNote = "bucket not searched: ";

/** What a reader can act on, from the transport error a bucket search leaves behind. */
const bucketReasons: [RegExp, string][] = [
  [
    /failed to lookup address|dns error|nodename nor servname|name or service not known/i,
    "주소를 찾지 못했습니다 (DNS 조회 실패)",
  ],
  [/connection refused/i, "연결이 거부되었습니다"],
  [/timed? ?out|timeout/i, "시간 안에 응답하지 않았습니다"],
  [/certificate|tls|ssl/i, "보안 연결을 맺지 못했습니다"],
  [
    /403|access ?denied|invalidaccesskeyid|signaturedoesnotmatch/i,
    "접근이 거부되었습니다",
  ],
  [/404|no ?such ?bucket/i, "버킷이 없습니다"],
];

const bucketReason = (detail: string) =>
  bucketReasons.find(([pattern]) => pattern.test(detail))?.[1] ??
  detail.split(": ")[0];

/** Unreachable buckets as written by the API (`bucket: error`) or a plan note, split
 * into the bucket, one short reason, and the error itself. */
export function bucketIssues(errors: string[] = []): BucketIssue[] {
  return errors.map((error) => {
    const text = error.startsWith(bucketNote)
      ? error.slice(bucketNote.length)
      : error;
    const cut = text.indexOf(": ");
    // The name is the first colon-delimited part when it is a bare label, not the error's prose.
    const name = cut > 0 ? text.slice(0, cut) : "";
    const bucket = name && !name.includes(" ") ? name : "";
    const detail = bucket ? text.slice(cut + 2) : text;
    return { bucket, reason: bucketReason(detail), detail };
  });
}

/** Buckets a release search could not reach, above the log that explains each one. */
function BucketNotice({ errors }: { errors: string[] }) {
  const issues = bucketIssues(errors);
  if (!issues.length) return null;
  return (
    <div className="bucket-notice">
      <div className="bucket-notice-head">
        <TriangleAlert size={16} className="shrink-0" />
        <span>
          버킷 {issues.length}개를 조회하지 못했습니다. 그 버킷의 템플릿은
          후보에서 빠지고, 찾은 템플릿이나 클린 설치로 계획합니다.
        </span>
      </div>
      <ul className="bucket-notice-list">
        {issues.map((issue) => (
          <li key={`${issue.bucket} ${issue.detail}`}>
            <code>{issue.bucket || "이름 없음"}</code>
            <span title={issue.detail}>{issue.reason}</span>
          </li>
        ))}
      </ul>
      <details className="bucket-notice-log">
        <summary>원문 로그 {issues.length}건</summary>
        {issues.map((issue) => (
          <pre key={issue.detail}>{issue.detail}</pre>
        ))}
      </details>
    </div>
  );
}

const computeSizes: { name: string; size: ComputeSize }[] = [
  { name: "기본", size: { cpus: 2, memory_mib: 3072, disk_mib: 32768 } },
  { name: "중간", size: { cpus: 4, memory_mib: 6144, disk_mib: 65536 } },
  { name: "큰", size: { cpus: 4, memory_mib: 10240, disk_mib: 65536 } },
  { name: "최대", size: { cpus: 8, memory_mib: 16384, disk_mib: 131072 } },
];

export function CreateFarmDialog({
  prominent = false,
  hostId,
  label = "Farm 만들기",
  disabled,
}: {
  prominent?: boolean;
  hostId?: string;
  label?: string;
  disabled?: boolean;
}) {
  const [open, setOpen] = useState(false);
  const [pending, setPending] = useState(false);
  return (
    <Dialog.Root
      open={open}
      onOpenChange={(next) => {
        if (!pending) setOpen(next);
      }}
    >
      <Dialog.Trigger
        disabled={disabled}
        render={
          <Button variant="primary" className={prominent ? "mt-5" : ""} />
        }
      >
        <Plus size={16} />
        {label}
      </Dialog.Trigger>
      {open && (
        <CreateFarmForm
          close={() => setOpen(false)}
          onPending={setPending}
          initialHost={hostId}
        />
      )}
    </Dialog.Root>
  );
}
export function CreateFarmForm({
  close,
  onPending,
  initialHost,
}: {
  close: () => void;
  onPending: (pending: boolean) => void;
  initialHost?: string;
}) {
  const client = useQueryClient();
  const navigate = useNavigate();
  const coordinator = useQuery(hostQuery());
  const hosts = useQuery(hostsQuery());
  const buildId = coordinator.data?.build_id;
  const [name, setName] = useState("");
  const [owner, setOwner] = useState("");
  const [pickedRelease, setPickedRelease] = useState<string>();
  const [selected, setSelected] = useState<string[]>([]);
  // A new farm always runs a control VM and the compute VMs it is made with; one compute
  // by default so a compute template can join it later.
  const [setup, setSetup] = useState("1");
  const computes = Number(setup);
  const [sizeIndex, setSizeIndex] = useState(0);
  // A virtual GPU on every compute; "none" means the farm has none.
  const [gpuMode, setGpuMode] = useState<GpuMode | "none">("none");
  const [gpuCount, setGpuCount] = useState("1");
  // A simulated GPU's own contract: how many fake cards the virtual GPUs sit on, and the
  // traits its flavor requires.
  const [gpuDevices, setGpuDevices] = useState("1");
  const [gpuTraits, setGpuTraits] = useState("CUSTOM_FARM_VGPU");
  // Compute VMs placed on another host than the control VM's (`compute-2` → host id).
  const [placed, setPlaced] = useState<Record<string, string>>({});
  // Host PCI devices compute-1 receives through VFIO (`0000:04:00.3`).
  const [selectedPci, setSelectedPci] = useState<string[]>([]);
  const compute = computeSizes[sizeIndex].size;
  // A compute VM runs Nova instances, so Nova always comes with it.
  const requested = selected.includes("nova")
    ? selected
    : [...selected, "nova"];
  // Until the user picks one, default to the given host, else the first ready one.
  const [picked, setPicked] = useState(initialHost);
  // Templates live on the local host, so it is the default for the control VM.
  const hostId =
    picked ??
    (
      hosts.data?.find((h) => h.kind === "local" && hostReady(h, buildId)) ??
      hosts.data?.find((h) => hostReady(h, buildId)) ??
      hosts.data?.[0]
    )?.id;
  const [editedHostName, setHostName] = useState<string>();
  const hostName = editedHostName ?? publicHostName(hostId ?? "", hosts.data);
  const target = hosts.data?.find((h) => h.id === hostId);
  // Releases and templates are offered for the control VM's host, not this coordinator.
  const hostArch =
    target?.inspection?.arch ??
    (target?.kind === "local" ? coordinator.data?.arch : undefined);
  const releases = useQuery(releasesQuery(hostArch));
  // One release for every VM: templates of it boot a VM when they match the services, and
  // only a release whose images run on the host is picked by default.
  const offered = releases.data?.releases ?? [];
  const release =
    pickedRelease ??
    [...offered]
      .filter((r) => runsOn(r, hostArch))
      .sort(
        (a, b) =>
          b.local_control +
          b.bucket_control -
          (a.local_control + a.bucket_control),
      )[0]?.release;
  const releaseInfo = offered.find((r) => r.release === release);
  // The catalog says which services the chosen release can run; it is asked again on change.
  const catalog = useQuery({
    ...catalogQuery(release),
    placeholderData: keepPreviousData,
  });
  // A release that cannot run a selected service drops it from the selection.
  useEffect(() => {
    const services = catalog.data?.components;
    if (!services) return;
    setSelected((current) => {
      const kept = current.filter(
        (id) => services.find((c) => c.id === id)?.available !== false,
      );
      return kept.length === current.length ? current : kept;
    });
  }, [catalog.data]);
  // Only the computes that exist, on hosts other than the control VM's.
  const placement = Object.fromEntries(
    Object.entries(placed).filter(
      ([node, host]) =>
        host !== hostId && Number(node.slice("compute-".length)) <= computes,
    ),
  );
  // The simulated mode writes Placement providers itself, which needs a provider with a
  // parent (microversion 1.14, long before 2025.1); the mtty mdev path needs every
  // compute's host to boot the custom kernel, and farm patches Nova's driver on the
  // releases before 2026.2. A GPU is a virtual device, never hardware. The coordinator
  // checks the same set of hosts when the farm is created (src/manager/mod.rs), so the
  // form asks each compute's host, not the control's.
  const mttyHosts = new Set(
    (hosts.data ?? [])
      .filter((h) => h.inspection?.mtty_available === true)
      .map((h) => h.id),
  );
  const computeHostIds = Array.from(
    { length: computes },
    (_, i) => placement[`compute-${i + 1}`] ?? hostId,
  );
  const mttyHost =
    computes > 0 && computeHostIds.every((id) => !!id && mttyHosts.has(id));
  // 2025.1 is the oldest release farm supports: its Placement takes a provider with a
  // parent, and farm's Nova backport gives it the mediated device.
  const gpuModeMinRelease = "2025.1";
  const gpuModeRelease = releaseAtLeast(release, gpuModeMinRelease);
  const gpuCountShown = Math.max(1, Math.min(24, Number(gpuCount) || 1));
  const gpuDevicesShown = Math.max(
    1,
    Math.min(gpuCountShown, Number(gpuDevices) || 1),
  );
  const gpuTraitsShown = gpuTraits
    .split(",")
    .map((trait) => trait.trim())
    .filter(Boolean);
  const gpu =
    gpuMode !== "none" &&
    computes > 0 &&
    gpuModeRelease &&
    (gpuMode !== "mtty" || mttyHost)
      ? gpuMode === "simulated"
        ? {
            mode: gpuMode,
            count: gpuCountShown,
            ...(gpuDevicesShown > 1 ? { devices: gpuDevicesShown } : {}),
            traits: gpuTraitsShown,
          }
        : { mode: gpuMode, count: gpuCountShown }
      : undefined;
  // What the request carries: a mode the release or a compute's host cannot express is not
  // sent, and the notice below says why instead of leaving the select on a silent mismatch.
  const gpuModeShown = gpu?.mode ?? "none";
  const missingMttyHosts = Array.from(
    new Set(
      computeHostIds.filter((id): id is string => !!id && !mttyHosts.has(id)),
    ),
  );
  // Why those hosts cannot bind an mtty vGPU now: their stored kernel and GPU probe say it.
  const missingMttyNotes = missingMttyHosts
    .map((id) => hosts.data?.find((h) => h.id === id))
    .filter((h): h is RemoteHost => !!h)
    .map((h) => `${h.id}: ${hostKernelNote(h)}`);
  const gpuProblem =
    gpuMode === "none" || gpu
      ? undefined
      : !gpuModeRelease
        ? `GPU(${gpuMode})는 OpenStack ${gpuModeMinRelease} 이상에서만 됩니다. 이 릴리스로는 GPU 없이 farm을 만듭니다.`
        : gpuMode === "mtty"
          ? `compute 호스트(${missingMttyHosts.join(", ") || hostId})가 mtty 커널 검증을 마치지 않아 GPU 없이 farm을 만듭니다. ${missingMttyNotes.join(" ")} Hosts에서 환경 준비를 실행하면 farm 커널 설치와 검증 VM의 mtty 확인까지 한 번에 끝납니다.`
          : undefined;
  const readyHosts = (hosts.data ?? []).filter((h) => hostReady(h, buildId));
  const canSpan =
    computes > 0 &&
    !!coordinator.data?.relay_url &&
    (hosts.data ?? []).filter((h) => hostReady(h, buildId)).length > 1;
  const farms = useQuery(farmsQuery());
  // PCI passthrough belongs to compute-1, so its host's inspection lists the devices. A device
  // an active farm on that host already passes through is taken, whatever its binding says.
  const pciHostId = computeHostIds[0];
  const pciInspection = hosts.data?.find((h) => h.id === pciHostId)?.inspection;
  const pciDevices: HostPciDevice[] = pciInspection?.pci_devices ?? [];
  const occupiedPci = new Map<string, string>();
  for (const f of farms.data ?? []) {
    if (f.host_id !== pciHostId || f.status === "deleted") continue;
    for (const bdf of f.plan?.pci_devices ?? []) occupiedPci.set(bdf, f.id);
  }
  const pciUsable = (bdf: string) =>
    computes > 0 &&
    pciInspection?.msb_pci_capable === true &&
    pciDevices.some((d) => d.bdf === bdf && d.vfio_ready === true) &&
    !occupiedPci.has(bdf);
  // Only devices still valid for this host and plan are sent; the effect drops the rest.
  const pciPicked = selectedPci.filter(pciUsable);
  const occupiedKey = Array.from(occupiedPci.keys()).join(",");
  useEffect(() => {
    setSelectedPci((current) => {
      const kept = current.filter(pciUsable);
      return kept.length === current.length ? current : kept;
    });
  }, [pciHostId, computes, pciInspection, occupiedKey]);
  const components = resolveComponents(
    catalog.data?.components ?? [],
    requested,
  );
  // Estimated footprints are planning guesses; the plan's memory already includes them.
  const estimated = (catalog.data?.components ?? []).some(
    (c) => c.estimated && components.includes(c.id),
  );
  const plan = useQuery({
    queryKey: ["plan", components, computes, compute, gpu, pciPicked],
    queryFn: ({ signal }) =>
      api.plan(components, computes, signal, compute, gpu, pciPicked),
    enabled: catalog.isSuccess,
    staleTime: Infinity,
  });
  // One release for the whole farm. The preview follows the ID once typing settles, so a
  // pause sends one request and the last plan stays on screen while the next one loads.
  const [planId, setPlanId] = useState("");
  useEffect(() => {
    const timer = setTimeout(() => setPlanId(name), 300);
    return () => clearTimeout(timer);
  }, [name]);
  const releaseFields: Omit<ReleaseCreateRequest, "id"> | undefined =
    release && hostId
      ? {
          release,
          host_id: hostId,
          components,
          computes,
          compute_hosts: Array.from(
            { length: computes },
            (_, i) => placement[`compute-${i + 1}`] ?? "",
          ),
          ...(compute ? { compute } : {}),
          ...(gpu ? { gpu } : {}),
          ...(pciPicked.length ? { pci_devices: pciPicked } : {}),
          external_host: hostName,
        }
      : undefined;
  const previewBody: ReleaseCreateRequest | undefined =
    releaseFields && farmId.test(planId)
      ? { ...releaseFields, id: planId }
      : undefined;
  const releasePlan = useQuery({
    queryKey: ["release-plan", previewBody],
    queryFn: ({ signal }) => api.releasePlan(previewBody!, signal),
    enabled: !!previewBody,
    retry: false,
    staleTime: 10_000,
    placeholderData: keepPreviousData,
  });
  // The catalog and the plan both name a bucket they could not search, once each.
  const bucketErrors = Array.from(
    new Set([
      ...(releases.data?.unreachable ?? []),
      ...(releasePlan.data?.notes ?? [])
        .filter((note) => note.startsWith(bucketNote))
        .map((note) => note.slice(bucketNote.length)),
    ]),
  );
  const mutation = useMutation({
    mutationFn: api.createRelease,
    onMutate: () => onPending(true),
    onSettled: () => onPending(false),
    onSuccess: async (job) => {
      await Promise.all([
        refresh(client),
        client.invalidateQueries({ queryKey: keys.snapshots }),
      ]);
      close();
      // The dialog closes on the farm it made: its page lists the assemble job and every step
      // under it while the farm still reads as deploying.
      await navigate({
        to: "/farms/$farmId",
        params: { farmId: job.farm_id },
      });
    },
  });
  const duplicate = farms.data?.some((f) => f.id === name);
  const capacity = target?.capacity;
  const remaining = capacity
    ? capacity.memory_budget_mib - capacity.memory_reserved_mib
    : null;
  // Computes placed elsewhere count against their own host.
  const placedMemory = plan.data
    ? Object.keys(placement).length * (plan.data.nodes[1]?.memory_mib ?? 0)
    : 0;
  const overBudget =
    remaining !== null &&
    plan.data &&
    plan.data.memory_mib - placedMemory > remaining;
  const targetError = hosts.error || coordinator.error;
  const noHosts = hosts.isSuccess && !hosts.data.length;
  const hostUnavailable = !target || !hostReady(target, buildId);
  const gpuModeSelect = (
    <SelectInput
      value={gpuModeShown}
      onChange={(value) => setGpuMode(value as GpuMode | "none")}
      disabled={mutation.isPending}
      label="GPU 모드"
      items={[
        { value: "none", label: "없음" },
        {
          value: "simulated",
          label: "simulated · 표준 VGPU (2025.1+)",
          disabled: !releaseAtLeast(release, "2025.1"),
        },
        {
          value: "mtty",
          label: `mtty · 커널 mdev (2025.1+)${mttyHost ? "" : " · 호스트 준비 필요"}`,
          disabled: !releaseAtLeast(release, "2025.1") || !mttyHost,
        },
      ]}
    />
  );
  return (
    <Modal
      title="새 farm 만들기"
      description="선택한 구성으로 VM을 만들고 OpenStack 서비스를 설치합니다."
      className="create-modal"
      closeDisabled={mutation.isPending}
    >
      <form
        onSubmit={(event) => {
          event.preventDefault();
          if (!releaseFields || !plan.data || duplicate || overBudget) return;
          if (hostUnavailable) return;
          mutation.mutate({
            ...releaseFields,
            id: name,
            ...(owner.trim() ? { owner: owner.trim() } : {}),
          });
        }}
      >
        <div className="modal-body">
          <div className="modal-form">
            <div>
              <span className="field-label">실행 호스트</span>
              {noHosts ? (
                <div className="warning-notice">
                  등록된 호스트가 없습니다.{" "}
                  <Link to="/hosts" className="text-link" onClick={close}>
                    Hosts
                  </Link>
                  에서 로컬 호스트나 원격 서버를 등록하고 환경 준비를 마치세요.
                </div>
              ) : (
                <>
                  <SelectInput
                    value={hostId ?? ""}
                    onChange={(value) => {
                      setPicked(value);
                      setHostName(undefined);
                    }}
                    disabled={mutation.isPending || !hosts.data}
                    label="실행 호스트"
                    items={(hosts.data ?? []).map((h) => {
                      const ready = hostReady(h, buildId);
                      return {
                        value: h.id,
                        label: `${hostLabel(h)}${ready ? "" : remoteAgentNeedsUpdate(h) ? " · 실행기 업데이트 필요" : " · 준비 필요"}`,
                        // Keep the current choice selectable so a preselected host that went stale still renders.
                        disabled: !ready && h.id !== hostId,
                      };
                    })}
                  />
                  <p className="field-hint mt-2">
                    {target?.kind === "local"
                      ? "coordinator(farmd) 머신에서 VM을 실행합니다"
                      : "선택한 서버에서 VM을 실행합니다"}
                    {remaining !== null &&
                      ` · 여유 ${memory(Math.max(0, remaining))}`}
                  </p>
                </>
              )}
            </div>
            <div className="grid gap-4 sm:grid-cols-2">
              <label className="field">
                <span>
                  Farm ID <span className="field-required">*</span>
                </span>
                <Input
                  value={name}
                  onValueChange={setName}
                  className="input font-mono"
                  placeholder="dev-openstack"
                  required
                  minLength={1}
                  maxLength={48}
                  pattern={"[a-z0-9]([a-z0-9\\-]{0,46}[a-z0-9])?"}
                  disabled={mutation.isPending}
                />
                <small className={duplicate ? "field-error" : undefined}>
                  {duplicate
                    ? "이미 사용 중인 Farm ID입니다."
                    : "소문자, 숫자, 하이픈 · 최대 48자"}
                </small>
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
                <small>환경을 관리하는 담당자를 표시합니다.</small>
              </label>
            </div>
            <div>
              <span className="field-label">VM 구성</span>
              <SelectInput
                value={setup}
                onChange={setSetup}
                disabled={mutation.isPending}
                label="VM 구성"
                items={[1, 2, 3].map((n) => ({
                  value: String(n),
                  label: `Control + Compute ${n}대`,
                }))}
              />
              <p className="field-hint mt-2">
                control은 API·DB·네트워크, compute는 Nova 인스턴스를 실행합니다.
              </p>
            </div>
            <div>
              <span className="field-label">Compute VM 사양</span>
              <SelectInput
                value={String(sizeIndex)}
                onChange={(value) => setSizeIndex(Number(value))}
                disabled={mutation.isPending}
                label="Compute VM 사양"
                items={computeSizes.map(({ name, size }, index) => ({
                  value: String(index),
                  label: `${name} · ${size.cpus} vCPU · 메모리 ${memory(size.memory_mib)} · 디스크 ${memory(size.disk_mib)}`,
                }))}
              />
              <p className="field-hint mt-2">
                compute VM마다 같은 사양이 적용됩니다 (약 0.8 GiB는 시스템용).
              </p>
            </div>
            <div>
              {gpuModeShown === "none" ? (
                <div>
                  <span className="field-label">GPU</span>
                  {gpuModeSelect}
                </div>
              ) : (
                <div className="grid gap-3 sm:grid-cols-2">
                  <div className="field">
                    <span>GPU</span>
                    {gpuModeSelect}
                  </div>
                  <label className="field">
                    <span>GPU 수량 (compute마다)</span>
                    <Input
                      value={gpuCount}
                      onValueChange={setGpuCount}
                      className="input"
                      type="number"
                      min={1}
                      max={24}
                      required
                      disabled={mutation.isPending}
                    />
                  </label>
                  {gpuModeShown === "simulated" && (
                    <>
                      <label className="field">
                        <span>가짜 GPU 장치 수 (compute마다)</span>
                        <Input
                          value={gpuDevices}
                          onValueChange={setGpuDevices}
                          className="input"
                          type="number"
                          min={1}
                          max={gpuCountShown}
                          disabled={mutation.isPending}
                        />
                      </label>
                      <label className="field">
                        <span>flavor가 요구할 trait (쉼표 구분)</span>
                        <Input
                          value={gpuTraits}
                          onValueChange={setGpuTraits}
                          className="input font-mono"
                          type="text"
                          placeholder="CUSTOM_FARM_VGPU"
                          disabled={mutation.isPending}
                        />
                      </label>
                    </>
                  )}
                </div>
              )}
              <p className="field-hint mt-2">
                실제 GPU가 아닌 가상 장치입니다. GPU farm은 클린 설치됩니다.
              </p>
              {gpuProblem && (
                <div className="warning-notice mt-2">{gpuProblem}</div>
              )}
            </div>
            {computes > 0 && pciInspection && (
              <div>
                {pciDevices.length > 0 &&
                pciInspection.pci_passthrough_available ? (
                  <>
                    <span className="field-label">
                      PCI 패스스루 (compute-1)
                    </span>
                    <div className="space-y-2">
                      {pciDevices.map((device) => {
                        const occupiedBy = occupiedPci.get(device.bdf);
                        const reason = occupiedBy
                          ? `사용 중(${occupiedBy})`
                          : device.vfio_ready
                            ? undefined
                            : "준비 안 됨";
                        const checked = pciPicked.includes(device.bdf);
                        return (
                          <label
                            key={device.bdf}
                            className="checkbox-row"
                            title={reason}
                            aria-disabled={reason ? true : undefined}
                          >
                            <Checkbox.Root
                              checked={checked}
                              disabled={!!reason || mutation.isPending}
                              onCheckedChange={(next) =>
                                setSelectedPci((current) =>
                                  next
                                    ? [...current, device.bdf]
                                    : current.filter(
                                        (bdf) => bdf !== device.bdf,
                                      ),
                                )
                              }
                              className="checkbox"
                            >
                              <Checkbox.Indicator>
                                <Check size={14} strokeWidth={3} />
                              </Checkbox.Indicator>
                            </Checkbox.Root>
                            <span>
                              <code className="font-mono">{device.bdf}</code>{" "}
                              <small className="font-mono">
                                {device.vendor_id}:{device.product_id}
                                {device.iommu_group != null &&
                                  ` · 그룹 ${device.iommu_group}`}
                              </small>
                              <small>{reason ?? "vfio-pci 준비됨"}</small>
                            </span>
                          </label>
                        );
                      })}
                    </div>
                    <p className="field-hint mt-2">
                      선택한 장치는 compute-1 VM에 직접 붙습니다. 장치 하나는
                      farm 하나만 쓸 수 있고, PCI farm은 클린 설치하며 스냅샷을
                      만들 수 없습니다.
                    </p>
                  </>
                ) : (
                  <p className="field-hint">
                    {pciDevices.length === 0
                      ? "compute-1 호스트에 vfio-pci로 바인딩된 PCI 장치가 없어 PCI 패스스루를 쓸 수 없습니다."
                      : "compute-1 호스트가 PCI 패스스루를 준비하지 않아 장치를 고를 수 없습니다. Hosts에서 점검하세요."}
                  </p>
                )}
              </div>
            )}
            {canSpan && (
              <div>
                <span className="field-label">VM 배치</span>
                <div className="grid gap-3 sm:grid-cols-2">
                  {/* The control VM's host is the farm's host, the same choice as 실행 호스트. */}
                  <SelectInput
                    value={hostId ?? ""}
                    onChange={(value) => {
                      setPicked(value);
                      setHostName(undefined);
                    }}
                    disabled={mutation.isPending}
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
                        value={placement[node] ?? hostId ?? ""}
                        onChange={(value) =>
                          setPlaced((current) => ({
                            ...current,
                            [node]: value,
                          }))
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
                <p className="field-hint mt-2">
                  control VM의 호스트는 위의 실행 호스트와 같습니다. 다른
                  호스트에 둔 compute VM은 coordinator의 relay(
                  {coordinator.data?.relay_url})를 거쳐 control VM과 같은 망에
                  연결되며, 링크는 종단 간 암호화됩니다.
                </p>
              </div>
            )}
            <div>
              <span className="field-label">OpenStack 릴리스</span>
              {releases.isPending ? (
                <Loading compact label="릴리스를 불러오는 중" />
              ) : releases.error ? (
                <ErrorNotice
                  error={releases.error}
                  retry={() => void releases.refetch()}
                />
              ) : (
                <>
                  <SelectInput
                    value={release ?? ""}
                    onChange={setPickedRelease}
                    disabled={mutation.isPending}
                    label="OpenStack 릴리스"
                    items={offered.map((r) => ({
                      value: r.release,
                      label: releaseLabel(r, hostArch),
                      // A selected release that the new host cannot run stays in the list so
                      // the warning below can name it; it is still not choosable here.
                      disabled: !runsOn(r, hostArch),
                    }))}
                  />
                  {releaseInfo &&
                    hostArch &&
                    !runsOn(releaseInfo, hostArch) && (
                      <div className="warning-notice mt-2">
                        OpenStack {releaseInfo.release}의 이미지는{" "}
                        {(releaseInfo.arches ?? []).map(archName).join("/")}용만
                        있어 {archName(hostArch)} 호스트({hostId})에서 만들 수
                        없습니다. 다른 릴리스나 호스트를 고르세요.
                      </div>
                    )}
                  <p className="field-hint mt-2">
                    템플릿이 있으면 사용하고, 없으면 클린 설치합니다.
                  </p>
                </>
              )}
              <BucketNotice errors={bucketErrors} />
            </div>
            <div>
              <span className="field-label">OpenStack 서비스</span>
              <div className="component-options">
                {catalog.data?.components.map((c) => {
                  const included = components.includes(c.id);
                  // A dependency of another selection stays on until that selection is removed.
                  const neededBy = catalog.data.components
                    .filter(
                      (other) =>
                        other.id !== c.id &&
                        requested.includes(other.id) &&
                        resolveComponents(catalog.data.components, [
                          other.id,
                        ]).includes(c.id),
                    )
                    .map((other) => componentLabel(other.id));
                  const forCompute = c.id === "nova";
                  const locked =
                    c.required ||
                    !c.available ||
                    forCompute ||
                    neededBy.length > 0;
                  return (
                    <label
                      className={`component-option ${included ? "selected" : ""} ${!c.available ? "unavailable" : ""}`}
                      key={c.id}
                      title={c.reason ?? undefined}
                    >
                      <Checkbox.Root
                        checked={included}
                        disabled={locked || mutation.isPending}
                        onCheckedChange={(checked) =>
                          setSelected((current) =>
                            checked
                              ? [...current, c.id]
                              : current.filter((id) => id !== c.id),
                          )
                        }
                        className="checkbox"
                      >
                        <Checkbox.Indicator>
                          <Check size={14} strokeWidth={3} />
                        </Checkbox.Indicator>
                      </Checkbox.Root>
                      <span className="component-text">
                        <span className="component-head">
                          <strong>{componentLabel(c.id)}</strong>
                          <span className="component-badge">
                            {c.required
                              ? "필수"
                              : !c.available
                                ? c.min_release
                                  ? `${c.min_release}+`
                                  : "준비 중"
                                : forCompute
                                  ? "Compute VM에 필요"
                                  : neededBy.length
                                    ? `${neededBy.join("·")}에 필요`
                                    : "선택"}
                          </span>
                        </span>
                        <small>{componentInfo[c.id]?.description}</small>
                      </span>
                    </label>
                  );
                })}
              </div>
              <p className="field-hint mt-2">
                의존성은 자동으로 포함됩니다. Nova를 뺀 서비스와 DB·메시지 큐는
                control VM의 컨테이너로 실행됩니다.
              </p>
            </div>
            <label className="field">
              <span>외부 접속 호스트</span>
              <Input
                value={hostName}
                onValueChange={setHostName}
                className="input font-mono"
                required
                maxLength={253}
                placeholder="localhost"
                disabled={mutation.isPending}
              />
              <small>
                OpenStack public URL에 사용할 VM 호스트의 DNS 이름 또는 IPv4
                주소
              </small>
            </label>
            {(catalog.isPending || plan.isPending) && !plan.data && (
              <Loading compact label="리소스 계획 계산 중" />
            )}
            {(catalog.error || plan.error || targetError) && (
              <ErrorNotice
                error={catalog.error || plan.error || targetError}
                retry={() => {
                  void catalog.refetch();
                  void plan.refetch();
                  void coordinator.refetch();
                  void hosts.refetch();
                }}
              />
            )}
            {overBudget && (
              <div className="warning-notice">
                예약 가능한 메모리가 부족합니다. 현재 여유:{" "}
                {memory(Math.max(0, remaining ?? 0))}. 다른 farm을 삭제하거나
                호스트의 예약 한도를 조절하세요.
              </div>
            )}
            {target && hostUnavailable && (
              <div className="warning-notice">
                {hostNotice(target, buildId)}
              </div>
            )}
            {mutation.error && <ErrorNotice error={mutation.error} />}
          </div>
          <aside className="modal-aside" aria-label="생성 계획">
            <section className="plan-summary">
              <span className="eyebrow">ENVIRONMENT PLAN</span>
              {!previewBody ? (
                <p>Farm ID를 입력하면 VM마다 어떻게 만들지 미리 보여 줍니다.</p>
              ) : releasePlan.isPending ? (
                <Loading compact label="계획을 확인하는 중" />
              ) : releasePlan.error ? (
                <p className="field-error">{releasePlan.error.message}</p>
              ) : releasePlan.data ? (
                <>
                  <ul>
                    {[
                      releasePlan.data.control,
                      ...releasePlan.data.computes,
                    ].map((node) => (
                      <li key={node.node}>
                        {node.source === "clean" ? (
                          <Sparkles size={16} />
                        ) : node.source === "bucket" ? (
                          <Download size={16} />
                        ) : (
                          <CopyPlus size={16} />
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
                    OpenStack {releasePlan.data.release} ·{" "}
                    {releasePlan.data.components.join(", ")}
                  </p>
                  {releasePlan.data.notes
                    ?.filter((note) => !note.startsWith(bucketNote))
                    .map((note) => (
                      <p key={note} className="muted">
                        {note}
                      </p>
                    ))}
                </>
              ) : null}
              {plan.data && (
                <div>
                  <span>
                    <Boxes size={17} />
                    <strong>{plan.data.nodes.length}</strong> VM
                  </span>
                  <span>
                    <Cpu size={17} />
                    <strong>{memory(plan.data.memory_mib)}</strong> 메모리
                    {estimated && <small className="muted"> · 추정 포함</small>}
                  </span>
                  <span>
                    <HardDrive size={17} />
                    <strong>{memory(plan.data.disk_mib)}</strong> 디스크
                  </span>
                </div>
              )}
            </section>
          </aside>
        </div>
        <div className="modal-footer">
          <span className="modal-footer-note flex items-center gap-1.5">
            <ShieldCheck size={16} />
            {`Farm 전용 VM ${computes + 1}대와 내부망`}
          </span>
          <div className="flex gap-2">
            <Dialog.Close render={<Button disabled={mutation.isPending} />}>
              취소
            </Dialog.Close>
            <Button
              type="submit"
              variant="primary"
              disabled={
                mutation.isPending ||
                !plan.data ||
                !releasePlan.data ||
                releasePlan.isFetching ||
                !capacity ||
                Boolean(targetError) ||
                duplicate ||
                Boolean(overBudget) ||
                Boolean(hostUnavailable)
              }
            >
              {mutation.isPending ? (
                <LoaderCircle size={16} className="animate-spin" />
              ) : (
                <Plus size={16} />
              )}
              {mutation.isPending ? "생성 요청 중" : "Farm 만들기"}
            </Button>
          </div>
        </div>
      </form>
    </Modal>
  );
}
export function DeployFarmDialog({
  farm,
  lastJob,
}: {
  farm: Farm;
  lastJob?: Job;
}) {
  const [open, setOpen] = useState(false);
  const [from, setFrom] = useState<Phase>("prepare");
  const client = useQueryClient();
  const mutation = useMutation({
    mutationFn: () => api.deploy(farm.id, from),
    onSuccess: async () => {
      await refresh(client);
      setOpen(false);
    },
  });
  const validNodes = isDeployableFarm(farm) && farmNodesRunning(farm);
  // Kolla came with the control template; such a farm only gains computes. A control VM
  // created clean still installs OpenStack on itself.
  const restoredControl = awaitsCompute(farm) && Boolean(farm.snapshot_id);
  const allowed =
    ["created", "failed", "ready"].includes(farm.status) &&
    validNodes &&
    !restoredControl &&
    !farm.active_job;
  const resumable =
    (lastJob?.status === "failed" || lastJob?.status === "interrupted") &&
    lastJob.kind === "deploy" &&
    Boolean(lastJob.phase);
  return (
    <Dialog.Root
      open={open}
      onOpenChange={(v) => {
        if (!mutation.isPending) {
          setOpen(v);
          if (v) {
            mutation.reset();
            setFrom(resumable ? lastJob!.phase! : "prepare");
          }
        }
      }}
    >
      <Dialog.Trigger
        render={<Button variant="primary" disabled={!allowed} />}
        title={
          !isDeployableFarm(farm)
            ? "기존 다중 VM Farm은 재배포할 수 없습니다. Farm을 새로 만드세요."
            : restoredControl
              ? "control 템플릿으로 만든 farm에는 아직 compute VM이 없습니다. Compute를 추가하세요."
              : undefined
        }
      >
        <Play size={16} />
        {farm.status === "creating"
          ? "VM 생성 중..."
          : farm.active_job
            ? "작업 진행 중"
            : resumable
              ? "배포 재개"
              : farm.status === "ready"
                ? "다시 배포"
                : "OpenStack 배포"}
      </Dialog.Trigger>
      <Modal
        title={resumable ? "OpenStack 배포 재개" : "OpenStack 배포"}
        description="Kolla 단계별 진행 상황과 로그를 실시간으로 확인할 수 있습니다."
        closeDisabled={mutation.isPending}
      >
        <form
          onSubmit={(event) => {
            event.preventDefault();
            mutation.mutate();
          }}
        >
          <div className="modal-body space-y-5">
            <div className="action-context">
              <Boxes size={20} />
              <div>
                <strong className="font-mono">{farm.id}</strong>
                <p>
                  {farmComponents(farm).map(componentLabel).join(" · ")} 배포
                </p>
              </div>
            </div>
            <label className="field">
              <span>시작 단계</span>
              <SelectInput
                label="배포 시작 단계"
                value={from}
                onChange={setFrom}
                items={phases.map((p) => ({
                  value: p.id,
                  label: `${p.label} — ${p.description}`,
                }))}
                disabled={mutation.isPending}
              />
            </label>
            <p className="field-hint">
              처음 배포할 때는 Prepare부터 시작하세요. 실패한 배포는 저장된
              단계부터 재개할 수 있습니다. 설정을 바꿨다면 configure부터
              재개하세요. 호스트당 배포는 한 번에 하나씩 실행됩니다.
            </p>
            {mutation.error && <ErrorNotice error={mutation.error} />}
          </div>
          <div className="modal-footer justify-end">
            <Dialog.Close render={<Button disabled={mutation.isPending} />}>
              취소
            </Dialog.Close>
            <Button
              type="submit"
              variant="primary"
              disabled={mutation.isPending}
            >
              {mutation.isPending ? (
                <LoaderCircle size={16} className="animate-spin" />
              ) : (
                <ArrowRight size={16} />
              )}
              배포 시작
            </Button>
          </div>
        </form>
      </Modal>
    </Dialog.Root>
  );
}
export function AddComputeDialog({
  farm,
  lastJob,
}: {
  farm: Farm;
  lastJob?: Job;
}) {
  const resumable =
    lastJob?.kind === "expand" &&
    ["failed", "interrupted"].includes(lastJob.status) &&
    Boolean(lastJob.target_node);
  const [open, setOpen] = useState(false);
  const [hostId, setHostId] = useState(
    lastJob?.target_host ?? farm.host_id ?? "",
  );
  const resumedTemplate = resumable ? (lastJob?.snapshot_id ?? "") : "";
  const [mode, setMode] = useState<"clean" | "snapshot">(
    resumedTemplate ? "snapshot" : "clean",
  );
  const [template, setTemplate] = useState(resumedTemplate);
  const client = useQueryClient();
  const hosts = useQuery(hostsQuery());
  const coordinator = useQuery(hostQuery());
  const snapshots = useQuery(snapshotsQuery());
  // The guest must boot on the new compute's host.
  const templates = computeTemplatesFor(
    farm,
    snapshots.data,
    hosts.data?.find((host) => host.id === hostId)?.inspection?.arch ??
      coordinator.data?.arch,
  );
  // A template boots on any host; one without a copy gets it when the job starts.
  const chosen = templates.find((snapshot) => snapshot.id === template);
  const copying = chosen
    ? hostId !== snapshotOrigin(chosen, hosts.data) &&
      !chosen.copies?.some((copy) => copy.host_id === hostId)
    : false;
  const plan = farm.plan;
  // Every compute joins the farm's own release; it cannot change with the VM being added.
  const release = plan?.release ?? null;
  const computes = plan?.computes ?? 0;
  const next = computes + 1;
  const ready = (hosts.data ?? []).filter((host) =>
    hostReady(host, coordinator.data?.build_id),
  );
  const remote = hostId !== farm.host_id;
  const relay = coordinator.data?.relay_url;
  const allowed =
    isDeployableFarm(farm) &&
    // A failed attempt leaves its VM running, and the inventory then no longer matches the
    // plan: the farm reads `broken` although the same expansion can go on where it stopped.
    (farm.status === "ready" || resumable) &&
    !farm.active_job &&
    isMultiNodeFarm(farm) &&
    computes < 3;
  const fromSnapshot = mode === "snapshot";
  const canSubmit =
    allowed &&
    Boolean(hostId) &&
    (!remote || Boolean(relay)) &&
    (!fromSnapshot || Boolean(template));
  const mutation = useMutation({
    mutationFn: () =>
      api.addCompute(
        farm.id,
        hostId || undefined,
        fromSnapshot ? template : undefined,
      ),
    onSuccess: async () => {
      await refresh(client);
      setOpen(false);
    },
  });
  return (
    <Dialog.Root
      open={open}
      onOpenChange={(value) => {
        if (!mutation.isPending) {
          setOpen(value);
          if (value) {
            mutation.reset();
            setHostId(lastJob?.target_host ?? farm.host_id ?? "");
            setMode(resumedTemplate ? "snapshot" : "clean");
            setTemplate(resumedTemplate);
          }
        }
      }}
    >
      <Dialog.Trigger
        render={<Button disabled={!allowed} />}
        title={
          computes >= 3
            ? "한 farm에는 compute VM을 3대까지 둘 수 있습니다."
            : !isMultiNodeFarm(farm)
              ? "all-in-one farm에는 compute를 추가할 수 없습니다. control과 compute로 이루어진 farm을 새로 만드세요."
              : resumable
                ? "실패한 compute 추가를 같은 VM·같은 방식으로 이어서 재시도합니다."
                : "배포가 끝난 farm에 다음 compute VM을 추가합니다."
        }
      >
        <Cpu size={16} />
        Compute 추가
      </Dialog.Trigger>
      <Modal
        title={`Compute ${next} 추가`}
        description="control VM은 그대로 두고, 새 compute VM만 준비해 Kolla에 등록합니다."
        closeDisabled={mutation.isPending}
      >
        <form
          onSubmit={(event) => {
            event.preventDefault();
            if (canSubmit) mutation.mutate();
          }}
        >
          <div className="modal-body space-y-5">
            <div className="action-context">
              <Cpu size={20} />
              <div>
                <strong className="font-mono">
                  {farm.id}-
                  {resumable && lastJob?.target_node
                    ? lastJob.target_node
                    : `compute-${next}`}
                </strong>
                <p>
                  기존 compute와 같은 크기 · {plan?.compute?.cpus ?? 2} vCPU ·{" "}
                  {memory(plan?.compute?.memory_mib ?? 3072)}
                </p>
              </div>
            </div>
            <div className="field">
              <span>OpenStack 릴리스</span>
              <span className="fixed-release">
                <Lock size={15} />
                OpenStack {release ?? "기록 없음"}
              </span>
              <small>
                farm의 릴리스는 바뀌지 않습니다. compute는 같은 릴리스로만
                붙으며, 클린 설치도 이 릴리스로 설치합니다. 다른 릴리스의
                compute 템플릿은 목록에 없고 보내도 거부됩니다.
              </small>
            </div>
            <div className="field">
              <span>설치 방식</span>
              <div className="segmented-control" role="radiogroup">
                {(
                  [
                    { value: "clean", label: "클린 설치" },
                    { value: "snapshot", label: "스냅샷에서" },
                  ] as const
                ).map((item) => (
                  <button
                    key={item.value}
                    type="button"
                    role="radio"
                    aria-checked={mode === item.value}
                    className={`flex-1 ${mode === item.value ? "selected" : ""}`}
                    disabled={mutation.isPending || resumable}
                    onClick={() => {
                      setMode(item.value);
                      if (item.value === "snapshot") {
                        if (!template && templates[0])
                          setTemplate(templates[0].id);
                      }
                    }}
                  >
                    {item.label}
                  </button>
                ))}
              </div>
            </div>
            {fromSnapshot && (
              <label className="field">
                <span>Compute 템플릿</span>
                <SelectInput
                  label="compute 템플릿 스냅샷"
                  value={template}
                  onChange={setTemplate}
                  items={(resumedTemplate && !templates.length
                    ? [{ id: resumedTemplate }]
                    : templates
                  ).map((snapshot) => ({
                    value: snapshot.id,
                    label: snapshot.id,
                  }))}
                  disabled={mutation.isPending || resumable}
                />
                <small>
                  이 farm과 같은 릴리스·서비스로 얼린 compute 템플릿만 보입니다.
                  다른 릴리스의 템플릿은 추가할 수 없습니다.
                </small>
              </label>
            )}
            {fromSnapshot && !templates.length && !resumedTemplate && (
              <div className="warning-notice">
                쓸 수 있는 compute 템플릿이 없습니다. 이 farm과 같은 릴리스
                {release ? `(OpenStack ${release})` : ""}·서비스로 얼린 compute
                템플릿만 붙일 수 있습니다. 배포된 farm의 얼리기에서 compute VM
                하나를 템플릿으로 얼려두세요.
              </div>
            )}
            <label className="field">
              <span>호스트</span>
              <SelectInput
                label={`compute-${next} 호스트`}
                value={hostId}
                onChange={setHostId}
                items={ready.map((host) => ({
                  value: host.id,
                  label: hostLabel(host),
                }))}
                disabled={mutation.isPending || resumable}
              />
            </label>
            {fromSnapshot && template && hostId && copying && (
              <div className="info-notice">
                <Cpu size={18} />
                <div>
                  이 호스트에는 템플릿 사본이 없어 작업이 먼저 복사합니다.
                  버킷에 올려둔 템플릿이면 호스트가 직접 내려받고, 아니면 이
                  coordinator가 보냅니다. 진행은 작업 로그에 나옵니다.
                </div>
              </div>
            )}
            {remote && !relay && (
              <div className="warning-notice">
                다른 호스트의 compute를 붙이려면 먼저 Hosts에서 호스트 간
                relay를 설정하세요.
              </div>
            )}
            {resumable && (
              <div className="info-notice">
                <RefreshCw size={18} />
                <div>
                  <strong>실패한 확장을 이어서 진행합니다.</strong> 새 VM은 이미
                  만들어졌을 수 있으며, 이 작업은 준비와 Kolla 등록부터 다시
                  실행합니다.
                </div>
              </div>
            )}
            <p className="field-hint">
              새 VM은 farm의 링크에 붙고, control VM의 Kolla inventory에
              추가됩니다. 기존 인스턴스는 새 compute로 자동 이동하지 않습니다.
              {fromSnapshot
                ? " 템플릿 디스크로 부팅해 노드 identity를 지우고, 이미지 pull 없이 Kolla reconfigure만 실행합니다."
                : " 다른 호스트를 고르면 중계 링크를 거칩니다."}
            </p>
            {mutation.error && <ErrorNotice error={mutation.error} />}
          </div>
          <div className="modal-footer justify-end">
            <Dialog.Close render={<Button disabled={mutation.isPending} />}>
              취소
            </Dialog.Close>
            <Button
              type="submit"
              variant="primary"
              disabled={!canSubmit || mutation.isPending}
            >
              {mutation.isPending ? (
                <LoaderCircle size={16} className="animate-spin" />
              ) : (
                <Plus size={16} />
              )}
              추가
            </Button>
          </div>
        </form>
      </Modal>
    </Dialog.Root>
  );
}
export function DeleteFarmDialog({ farm }: { farm: Farm }) {
  const [open, setOpen] = useState(false);
  const [confirm, setConfirm] = useState("");
  const [forget, setForget] = useState(false);
  const client = useQueryClient();
  const mutation = useMutation({
    mutationFn: () => api.delete(farm.id, forget),
    onSuccess: async () => {
      await refresh(client);
      setOpen(false);
    },
  });
  return (
    <Dialog.Root
      open={open}
      onOpenChange={(v) => {
        if (!mutation.isPending) {
          setOpen(v);
          if (v) {
            setConfirm("");
            setForget(false);
            mutation.reset();
          }
        }
      }}
    >
      <Dialog.Trigger
        render={
          <Button
            variant="ghost"
            disabled={Boolean(farm.active_job) || farm.status === "deleted"}
            className="delete-trigger"
          />
        }
        aria-label="Farm 삭제"
      >
        <Trash2 size={16} />
        Farm 삭제
      </Dialog.Trigger>
      <Modal
        title="Farm을 삭제할까요?"
        description={
          forget
            ? "이 farm의 coordinator 기록만 삭제합니다."
            : "이 farm의 모든 VM과 연결된 디스크가 삭제됩니다."
        }
        closeDisabled={mutation.isPending}
      >
        <form
          onSubmit={(event) => {
            event.preventDefault();
            if (confirm === farm.id) mutation.mutate();
          }}
        >
          <div className="modal-body space-y-5">
            <div className="warning-notice">
              이 작업은 되돌릴 수 없습니다. 필요한 이미지나 데이터를 먼저
              보관하세요.
            </div>
            <ForgetOption
              checked={forget}
              onChange={setForget}
              disabled={mutation.isPending}
              left="VM과 디스크"
            />
            <div className="field">
              <span>
                확인을 위해 <CopyText value={farm.id} /> 입력
              </span>
              <Input
                aria-label={`확인을 위해 ${farm.id} 입력`}
                className="input font-mono"
                value={confirm}
                onValueChange={setConfirm}
                placeholder={farm.id}
                disabled={mutation.isPending}
                autoComplete="off"
              />
            </div>
            {mutation.error && <ErrorNotice error={mutation.error} />}
          </div>
          <div className="modal-footer justify-end">
            <Dialog.Close render={<Button disabled={mutation.isPending} />}>
              취소
            </Dialog.Close>
            <Button
              type="submit"
              variant="danger"
              disabled={confirm !== farm.id || mutation.isPending}
            >
              {mutation.isPending ? (
                <LoaderCircle size={16} className="animate-spin" />
              ) : (
                <Trash2 size={16} />
              )}
              {forget ? "기록만 삭제" : "Farm 삭제"}
            </Button>
          </div>
        </form>
      </Modal>
    </Dialog.Root>
  );
}

/** Re-checks a deployed farm's services after its VM started from disk; nothing is deployed. */
export function CheckServicesButton({
  farm,
  deployed,
}: {
  farm: Farm;
  deployed: boolean;
}) {
  const client = useQueryClient();
  const mutation = useMutation({
    mutationFn: () => api.settle(farm.id),
    onSuccess: () => refresh(client),
  });
  const allowed =
    deployed &&
    !farm.active_job &&
    isDeployableFarm(farm) &&
    ["created", "broken", "ready"].includes(farm.status) &&
    farmNodesRunning(farm);
  return (
    <Button
      onClick={() => mutation.mutate()}
      disabled={!allowed || mutation.isPending}
      title={
        deployed
          ? "VM을 다시 시작한 뒤 서비스가 응답하는지 확인합니다. 배포는 다시 실행하지 않습니다."
          : "배포를 한 번 완료한 farm에서 사용할 수 있습니다."
      }
    >
      {mutation.isPending ? (
        <LoaderCircle size={16} className="animate-spin" />
      ) : (
        <RefreshCw size={16} />
      )}
      서비스 확인
    </Button>
  );
}
