import { Button as BaseButton } from "@base-ui/react/button";
import { Checkbox } from "@base-ui/react/checkbox";
import { Dialog } from "@base-ui/react/dialog";
import { Select } from "@base-ui/react/select";
import {
  Check,
  ChevronDown,
  CircleAlert,
  Copy,
  LoaderCircle,
  RefreshCw,
  X,
} from "lucide-react";
import {
  useEffect,
  useRef,
  useState,
  type ComponentProps,
  type ReactNode,
} from "react";

export function Button({
  variant = "default",
  className = "",
  ...props
}: ComponentProps<typeof BaseButton> & {
  variant?: "default" | "primary" | "danger" | "ghost";
}) {
  return (
    <BaseButton className={`btn btn-${variant} ${className}`} {...props} />
  );
}
const labels: Record<string, string> = {
  registered: "등록됨",
  checking: "점검 중",
  preparing: "준비 중",
  needs_setup: "준비 필요",
  runtime_ready: "Runtime 확인됨",
  created: "VM 준비됨",
  creating: "생성 중",
  deploying: "배포 중",
  snapshotting: "얼리는 중",
  settling: "응답 확인 중",
  ready: "Ready",
  failed: "실패",
  broken: "점검 필요",
  deleting: "삭제 중",
  deleted: "삭제됨",
  exporting: "내보내는 중",
  importing: "가져오는 중",
  queued: "대기 중",
  running: "실행 중",
  succeeded: "완료",
  interrupted: "중단됨",
};
export function StatusBadge({ status }: { status: string }) {
  return (
    <span className={`status-badge status-${status}`}>
      <span className="status-dot" />
      {labels[status] ?? status}
    </span>
  );
}
export function ErrorNotice({
  error,
  retry,
}: {
  error: unknown;
  retry?: () => void;
}) {
  return (
    <div role="alert" className="error-notice">
      <CircleAlert size={17} className="shrink-0" />
      <div className="min-w-0 flex-1">
        <strong>요청을 완료하지 못했습니다</strong>
        <p className="break-words">
          {error instanceof Error ? error.message : String(error)}
        </p>
      </div>
      {retry && (
        <Button onClick={retry} variant="ghost">
          <RefreshCw size={14} />
          다시 시도
        </Button>
      )}
    </div>
  );
}
/** `compact` keeps the loader to one line, for panels whose height must not change. */
export function Loading({
  label = "불러오는 중",
  compact,
}: {
  label?: string;
  compact?: boolean;
}) {
  return (
    <div
      role="status"
      className={compact ? "loading loading-compact" : "loading"}
    >
      <LoaderCircle size={compact ? 15 : 20} className="animate-spin" />
      {label}
    </div>
  );
}
export function PageHeading({
  eyebrow,
  title,
  description,
  actions,
}: {
  eyebrow?: string;
  title: string;
  description: string;
  actions?: ReactNode;
}) {
  return (
    <div className="page-heading">
      <div>
        {eyebrow && <p className="eyebrow">{eyebrow}</p>}
        <h1>{title}</h1>
        <p className="page-description">{description}</p>
      </div>
      <div className="flex shrink-0 flex-wrap gap-2">{actions}</div>
    </div>
  );
}
export function Modal({
  title,
  description,
  children,
  className = "",
  closeDisabled,
}: {
  title: string;
  description: string;
  children: ReactNode;
  className?: string;
  closeDisabled?: boolean;
}) {
  // Focus the dialog itself: leaving it to the first button rings the close icon on open.
  const popup = useRef<HTMLDivElement>(null);
  return (
    <Dialog.Portal>
      <Dialog.Backdrop className="modal-backdrop" />
      <Dialog.Viewport className="modal-viewport">
        <Dialog.Popup
          ref={popup}
          initialFocus={popup}
          className={`modal ${className}`}
        >
          <div className="modal-heading">
            <div>
              <Dialog.Title className="modal-title">{title}</Dialog.Title>
              <Dialog.Description className="muted mt-1 text-sm">
                {description}
              </Dialog.Description>
            </div>
            <Dialog.Close
              className="icon-button"
              aria-label="닫기"
              disabled={closeDisabled}
            >
              <X size={18} />
            </Dialog.Close>
          </div>
          {children}
        </Dialog.Popup>
      </Dialog.Viewport>
    </Dialog.Portal>
  );
}
export function SelectInput<T extends string>({
  value,
  onChange,
  items,
  label,
  disabled,
}: {
  value: T;
  onChange: (value: T) => void;
  items: { value: T; label: string; disabled?: boolean }[];
  label: string;
  disabled?: boolean;
}) {
  return (
    <Select.Root
      value={value}
      onValueChange={(v) => {
        if (v !== null) onChange(v as T);
      }}
      items={items}
      disabled={disabled}
    >
      <Select.Trigger
        className="select-trigger"
        aria-label={label}
        title={items.find((item) => item.value === value)?.label ?? label}
      >
        <Select.Value />
        <Select.Icon>
          <ChevronDown size={15} />
        </Select.Icon>
      </Select.Trigger>
      <Select.Portal>
        <Select.Positioner sideOffset={6} className="z-[80]">
          <Select.Popup className="select-popup">
            <Select.List>
              {items.map((item) => (
                <Select.Item
                  value={item.value}
                  key={item.value}
                  disabled={item.disabled}
                  className="select-item"
                >
                  <Select.ItemText>{item.label}</Select.ItemText>
                  <Select.ItemIndicator>
                    <Check size={15} />
                  </Select.ItemIndicator>
                </Select.Item>
              ))}
            </Select.List>
          </Select.Popup>
        </Select.Positioner>
      </Select.Portal>
    </Select.Root>
  );
}
export function EmptyState({
  icon,
  title,
  description,
  action,
}: {
  icon: ReactNode;
  title: string;
  description: string;
  action?: ReactNode;
}) {
  return (
    <div className="empty-state">
      <div className="empty-icon">{icon}</div>
      <h2>{title}</h2>
      <p>{description}</p>
      {action}
    </div>
  );
}

/** Inline value that copies itself; falls back to selecting the text when clipboard access is denied.
 *  `hideValue` keeps a secret out of the DOM and shows only the copy affordance. */
export function CopyText({
  value,
  hideValue = false,
  label,
  name,
}: {
  value: string;
  hideValue?: boolean;
  label?: string;
  /** Accessible name when `label` already ends in its own wording. */
  name?: string;
}) {
  const [copied, setCopied] = useState(false);
  const timeout = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  useEffect(() => () => clearTimeout(timeout.current), []);
  return (
    <button
      type="button"
      className={`copy-text ${hideValue ? "copy-only" : ""}`}
      title={copied ? "복사됨" : "클릭해서 복사"}
      aria-label={`${name ?? label ?? (hideValue ? "값" : value)} 복사`}
      onClick={async (event) => {
        const target = event.currentTarget;
        try {
          await navigator.clipboard.writeText(value);
          setCopied(true);
          clearTimeout(timeout.current);
          timeout.current = setTimeout(() => setCopied(false), 1500);
        } catch {
          window.getSelection()?.selectAllChildren(target);
        }
      }}
    >
      {!hideValue && <span>{label ?? value}</span>}
      {copied ? <Check size={12} /> : <Copy size={12} />}
    </button>
  );
}

/** Opts a delete out of asking the host, for a host that is gone or unreachable. */
export function ForgetOption({
  checked,
  onChange,
  disabled,
  left,
}: {
  checked: boolean;
  onChange: (checked: boolean) => void;
  disabled?: boolean;
  /** What stays on the host, e.g. "VM과 디스크". */
  left: string;
}) {
  return (
    <label className="checkbox-row">
      <Checkbox.Root
        checked={checked}
        disabled={disabled}
        onCheckedChange={(next) => onChange(next === true)}
        className="checkbox"
      >
        <Checkbox.Indicator>
          <Check size={12} strokeWidth={3} />
        </Checkbox.Indicator>
      </Checkbox.Root>
      <span>
        기록만 삭제
        <small>
          호스트에 묻지 않고 coordinator 기록만 지웁니다. 호스트에 남은 {left}는
          그대로 두니, 호스트가 없어졌거나 접속되지 않을 때만 쓰세요.
        </small>
      </span>
    </label>
  );
}
