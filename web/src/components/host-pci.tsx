import { Dialog } from "@base-ui/react/dialog";
import { useQuery } from "@tanstack/react-query";
import {
  Check,
  ChevronDown,
  Copy,
  LoaderCircle,
  RefreshCw,
  ShieldCheck,
} from "lucide-react";
import { useEffect, useRef, useState, type ReactNode } from "react";
import {
  fetchHostPciHelperInstall,
  hostJobsQuery,
  useBindHostPci,
  useUnbindHostPci,
  type HostJob,
  type HostPciCandidate,
  type RemoteHost,
} from "../api/client";
import { Button, ErrorNotice, Loading, Modal } from "./ui";

const CLASS_TAGS: Record<string, string> = {
  gpu: "GPU",
  network: "Network",
  audio: "Audio",
  nvme: "NVMe",
  storage: "Storage",
  usb: "USB",
  other: "Other",
};

const PROTECTED_LABELS: Record<string, string> = {
  no_iommu_group: "IOMMU 그룹 없음",
  shared_iommu_group: "IOMMU 그룹 미분리",
  root_disk: "루트 디스크",
  default_route_nic: "기본 라우트 NIC",
  usb_in_use: "USB 장치 연결 중",
  bound_to_stackbox: "Stackbox 점유 중",
  unknown: "확인 불가",
};

/** The Korean label for a `protected_reason` the coordinator reports. */
export function protectedLabel(reason: string) {
  return PROTECTED_LABELS[reason] ?? reason;
}

/** Manual commands for a host without the helper. They mirror what the helper does. */
export function bindCommand(device: HostPciCandidate) {
  const { bdf } = device;
  const lines = ["sudo modprobe vfio-pci"];
  lines.push(
    `echo vfio-pci | sudo tee /sys/bus/pci/devices/${bdf}/driver_override`,
  );
  if (device.driver)
    lines.push(
      `echo ${bdf} | sudo tee /sys/bus/pci/devices/${bdf}/driver/unbind`,
    );
  lines.push(`echo ${bdf} | sudo tee /sys/bus/pci/drivers/vfio-pci/bind`);
  if (device.iommu_group != null)
    lines.push(
      `sudo chgrp kvm /dev/vfio/${device.iommu_group}`,
      `sudo chmod g+rw /dev/vfio/${device.iommu_group}`,
    );
  return lines.join("\n");
}

export function unbindCommand(bdf: string) {
  return [
    `echo ${bdf} | sudo tee /sys/bus/pci/drivers/vfio-pci/unbind`,
    `echo | sudo tee /sys/bus/pci/devices/${bdf}/driver_override`,
    `echo ${bdf} | sudo tee /sys/bus/pci/drivers_probe`,
  ].join("\n");
}

/** Copies a command or script and says so for a moment. */
function CopyCommandButton({
  label,
  value,
  disabled,
}: {
  label: string;
  value: string;
  disabled?: boolean;
}) {
  const [copied, setCopied] = useState(false);
  const [failed, setFailed] = useState(false);
  const timeout = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  useEffect(() => () => clearTimeout(timeout.current), []);
  return (
    <Button
      disabled={disabled}
      onClick={async () => {
        try {
          await navigator.clipboard.writeText(value);
          setFailed(false);
          setCopied(true);
        } catch {
          setFailed(true);
        }
        clearTimeout(timeout.current);
        timeout.current = setTimeout(() => {
          setCopied(false);
          setFailed(false);
        }, 1500);
      }}
    >
      {copied ? <Check size={15} /> : <Copy size={15} />}
      {copied ? "복사됨" : failed ? "복사 실패" : label}
    </Button>
  );
}

/** The helper's install script, with the paths it writes. A non-SSH host names its login user. */
function HelperInstallPanel({
  host,
  onCheck,
  checkDisabled,
}: {
  host: RemoteHost;
  onCheck?: () => void;
  checkDisabled?: boolean;
}) {
  const ssh = host.kind === "ssh";
  const checkLabel = ssh ? "SSH 점검" : "점검";
  const [name, setName] = useState("");
  const [submitted, setSubmitted] = useState<string>();
  const script = useQuery({
    queryKey: ["host-pci-helper-install", host.id, ssh ? null : submitted],
    queryFn: () =>
      fetchHostPciHelperInstall(host.id, ssh ? undefined : submitted),
    enabled: ssh || !!submitted,
    staleTime: Infinity,
    retry: false,
  });
  return (
    <div className="remote-pci-helper">
      {!ssh && (
        <form
          className="remote-pci-helper-form"
          onSubmit={(event) => {
            event.preventDefault();
            const user = name.trim();
            if (user) setSubmitted(user);
          }}
        >
          <label>
            <span>호스트 사용자명</span>
            <input
              value={name}
              onChange={(event) => setName(event.target.value)}
              placeholder="예: ubuntu"
              autoComplete="off"
            />
          </label>
          <Button type="submit" disabled={!name.trim()}>
            스크립트 불러오기
          </Button>
        </form>
      )}
      {script.isFetching && <Loading compact label="스크립트 준비 중" />}
      {script.error && <ErrorNotice error={script.error} />}
      {script.data && (
        <>
          <p className="field-hint">
            호스트에서 root 권한 셸로 실행하세요. 스크립트는 헬퍼와 sudoers 규칙을
            설치하고 검증한 뒤 결과를 출력합니다.
          </p>
          <pre className="remote-pci-script" aria-label="헬퍼 설치 스크립트">
            <code>{script.data.script}</code>
          </pre>
          <div className="remote-pci-helper-foot">
            <span>
              <code>{script.data.helper_path}</code>
              <code>{script.data.sudoers_path}</code>
            </span>
            <CopyCommandButton label="스크립트 복사" value={script.data.script} />
          </div>
          {onCheck && (
            <div className="remote-pci-check">
              <p className="field-hint">
                {`설치 후 상단 '${checkLabel}' 버튼을 누르면 원클릭 바인딩이 활성화됩니다.`}
              </p>
              <Button disabled={checkDisabled} onClick={onCheck}>
                <RefreshCw size={15} />
                {`${checkLabel} 실행`}
              </Button>
            </div>
          )}
        </>
      )}
    </div>
  );
}

type Confirm = { kind: "bind" | "unbind"; device: HostPciCandidate };

/** Bound to `vfio-pci` or bindable: what the stackbox can take now, shown first. */
const isPrimary = (d: HostPciCandidate) =>
  d.driver === "vfio-pci" || d.bindable === true;

function ClassTag({ name }: { name: string }) {
  return (
    <span className="remote-pci-class">{CLASS_TAGS[name] ?? "Other"}</span>
  );
}

function PciCandidateRow({
  device,
  helper,
  busy,
  onConfirm,
}: {
  device: HostPciCandidate;
  helper: boolean;
  busy: boolean;
  onConfirm: (confirm: Confirm) => void;
}) {
  const owned = Boolean(device.owning_stackbox);
  const vfio = device.driver === "vfio-pci";
  const protectedBy = device.protected_reason;
  let action: ReactNode = null;
  if (vfio) {
    if (owned) action = <Button disabled>원복</Button>;
    else
      action = helper ? (
        <Button
          disabled={busy}
          onClick={() => onConfirm({ kind: "unbind", device })}
        >
          원복
        </Button>
      ) : (
        <CopyCommandButton
          label="원복 명령 복사"
          value={unbindCommand(device.bdf)}
        />
      );
  } else if (device.bindable && !protectedBy) {
    action = helper ? (
      <Button
        variant="primary"
        disabled={busy}
        onClick={() => onConfirm({ kind: "bind", device })}
      >
        vfio-pci 바인딩
      </Button>
    ) : (
      <CopyCommandButton label="바인딩 명령 복사" value={bindCommand(device)} />
    );
  }
  return (
    <li className="remote-pci-row">
      <div className="remote-pci-info">
        <div className="remote-pci-line">
          <code className="remote-pci-bdf">{device.bdf}</code>
          <code className="remote-pci-id">
            {device.vendor_id}:{device.product_id}
          </code>
          <ClassTag name={device.class_name} />
          {device.warning === "boot_vga" && (
            <span
              className="remote-pci-badge tone-warn"
              title="부팅 화면을 출력하는 GPU입니다"
            >
              디스플레이 GPU
            </span>
          )}
        </div>
        <div className="remote-pci-line remote-pci-meta">
          <span>
            {device.iommu_group != null
              ? `IOMMU 그룹 ${device.iommu_group}`
              : "IOMMU 그룹 없음"}
          </span>
          <span>
            드라이버{" "}
            <code>{device.driver || "드라이버 없음"}</code>
          </span>
          {device.vfio_ready && (
            <span className="remote-pci-badge tone-ok">vfio 준비됨</span>
          )}
          {owned && (
            <span className="remote-pci-badge tone-accent">
              사용 중({device.owning_stackbox})
            </span>
          )}
          {protectedBy && (
            <span className="remote-pci-badge tone-bad">
              {protectedLabel(protectedBy)}
            </span>
          )}
        </div>
      </div>
      {action && <div className="remote-pci-action">{action}</div>}
    </li>
  );
}

/**
 * The result of the last bind or unbind this panel started, read from the host's job so it reports
 * what the helper actually did. A succeeded job is the success banner; a failed one says why.
 */
function PciJobNotice({
  job,
  onDismiss,
}: {
  job: HostJob;
  onDismiss: () => void;
}) {
  const bdf = job.pci_bdf ?? "";
  const binding = job.kind === "pci_bind";
  if (job.status === "succeeded")
    return (
      <div className="remote-pci-banner tone-ok" role="status">
        <span>
          {`✓ ${bdf} ${binding ? "vfio-pci 바인딩이 완료되었습니다." : "원래 드라이버로 원복되었습니다."}`}
        </span>
        <Button onClick={onDismiss}>닫기</Button>
      </div>
    );
  if (job.status === "failed" || job.status === "interrupted")
    return (
      <div className="remote-pci-banner tone-bad" role="alert">
        <span>
          {`${bdf} ${binding ? "vfio-pci 바인딩" : "원복"}이 실패했습니다.${job.error ? ` ${job.error}` : ""}`}
        </span>
        <Button onClick={onDismiss}>닫기</Button>
      </div>
    );
  return (
    <div className="remote-pci-banner tone-accent" role="status">
      <LoaderCircle size={16} className="animate-spin" />
      <span>{`${bdf} ${binding ? "바인딩" : "원복"} 진행 중…`}</span>
    </div>
  );
}

/** A host's PCI devices with the bind and unbind actions. Used once the coordinator reports candidates. */
export function PciCandidates({
  host,
  onCheck,
  checkDisabled,
}: {
  host: RemoteHost;
  /** Runs the host check ("SSH 점검"), the step after installing the helper. */
  onCheck?: () => void;
  checkDisabled?: boolean;
}) {
  const candidates = host.inspection?.pci_candidates ?? [];
  const helper = host.inspection?.pci_bind_helper_available === true;
  const [helperOpen, setHelperOpen] = useState(false);
  const [protectedOpen, setProtectedOpen] = useState(false);
  const [confirm, setConfirm] = useState<Confirm>();
  const [error, setError] = useState<unknown>();
  const [submitted, setSubmitted] = useState<HostJob>();
  const jobs = useQuery(hostJobsQuery());
  const tracked =
    submitted && (jobs.data?.find((j) => j.id === submitted.id) ?? submitted);
  const bind = useBindHostPci();
  const unbind = useUnbindHostPci();
  const pending = bind.isPending || unbind.isPending;
  const busy = pending || !!host.active_job;
  const primary = candidates.filter(isPrimary);
  const rest = candidates.filter((d) => !isPrimary(d));
  const run = async () => {
    if (!confirm) return;
    setError(undefined);
    try {
      const mutation = confirm.kind === "bind" ? bind : unbind;
      const job = await mutation.mutateAsync({
        hostId: host.id,
        bdf: confirm.device.bdf,
      });
      setSubmitted(job);
      setConfirm(undefined);
    } catch (e) {
      setError(e);
    }
  };
  const renderRow = (device: HostPciCandidate) => (
    <PciCandidateRow
      key={device.bdf}
      device={device}
      helper={helper}
      busy={busy}
      onConfirm={(next) => {
        setError(undefined);
        setConfirm(next);
      }}
    />
  );
  const device = confirm?.device;
  return (
    <section className="remote-pci" aria-label="PCI 패스스루 장치">
      <div className="remote-pci-head">
        <h4 className="remote-pci-title">PCI 패스스루 장치</h4>
        {helper ? (
          <span className="remote-pci-ready">
            <ShieldCheck size={14} />
            원클릭 바인딩 준비됨
          </span>
        ) : (
          <Button
            aria-expanded={helperOpen}
            onClick={() => setHelperOpen((open) => !open)}
          >
            원클릭 헬퍼 설치
          </Button>
        )}
      </div>
      {tracked && (
        <PciJobNotice job={tracked} onDismiss={() => setSubmitted(undefined)} />
      )}
      {!helper && helperOpen && (
        <HelperInstallPanel
          host={host}
          onCheck={onCheck}
          checkDisabled={checkDisabled}
        />
      )}
      {primary.length ? (
        <ul className="remote-pci-list">{primary.map(renderRow)}</ul>
      ) : (
        <p className="field-hint">
          바인딩 가능하거나 <code>vfio-pci</code>에 바인딩된 장치가 없습니다.
        </p>
      )}
      {rest.length > 0 && (
        <>
          <button
            type="button"
            className="remote-pci-toggle"
            aria-expanded={protectedOpen}
            onClick={() => setProtectedOpen((open) => !open)}
          >
            {protectedOpen
              ? "시스템 보호·미분리 장치 접기"
              : `시스템 보호·미분리 장치 ${rest.length}개 보기`}
            <ChevronDown
              size={15}
              className={protectedOpen ? "remote-pci-chevron open" : "remote-pci-chevron"}
            />
          </button>
          {protectedOpen && (
            <ul className="remote-pci-list remote-pci-protected">
              {rest.map(renderRow)}
            </ul>
          )}
        </>
      )}
      <Dialog.Root
        open={!!confirm}
        onOpenChange={(next) => {
          if (pending) return;
          if (!next) {
            setConfirm(undefined);
            setError(undefined);
          }
        }}
      >
        {confirm && device && (
          <Modal
            title={
              confirm.kind === "bind"
                ? `${device.bdf} vfio-pci 바인딩`
                : `${device.bdf} 원복`
            }
            description={
              confirm.kind === "bind"
                ? "호스트 드라이버에서 떼어 vfio-pci에 넘깁니다. 이 장치를 쓰는 프로세스가 있으면 먼저 멈추세요."
                : "vfio-pci에서 떼어 호스트 드라이버로 되돌립니다. Stackbox가 쓰는 중이면 원복할 수 없습니다."
            }
            closeDisabled={pending}
          >
            <div className="modal-body space-y-4">
              <dl className="build-compare">
                <div>
                  <dt>BDF</dt>
                  <dd>
                    <code>{device.bdf}</code>
                  </dd>
                </div>
                <div>
                  <dt>클래스</dt>
                  <dd>
                    {CLASS_TAGS[device.class_name] ?? "Other"} ·{" "}
                    <code>{device.class}</code>
                  </dd>
                </div>
                <div>
                  <dt>현재 드라이버</dt>
                  <dd>
                    <code>{device.driver || "드라이버 없음"}</code>
                  </dd>
                </div>
                <div>
                  <dt>IOMMU 그룹</dt>
                  <dd>
                    {device.iommu_group != null
                      ? device.iommu_group
                      : "없음"}
                  </dd>
                </div>
              </dl>
              {device.warning === "boot_vga" && (
                <div className="warning-notice">
                  부팅 화면을 출력하는 디스플레이 GPU입니다. 바인딩하면 이 호스트의
                  콘솔 화면이 나오지 않을 수 있습니다.
                </div>
              )}
              {error ? <ErrorNotice error={error} /> : null}
            </div>
            <div className="modal-footer justify-end">
              <Dialog.Close render={<Button disabled={pending} />}>
                취소
              </Dialog.Close>
              <Button
                variant={confirm.kind === "bind" ? "primary" : "danger"}
                disabled={pending}
                onClick={run}
              >
                {pending && <LoaderCircle size={16} className="animate-spin" />}
                {confirm.kind === "bind" ? "바인딩 실행" : "원복 실행"}
              </Button>
            </div>
          </Modal>
        )}
      </Dialog.Root>
    </section>
  );
}
