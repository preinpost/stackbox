import { Checkbox } from "@base-ui/react/checkbox";
import { Dialog } from "@base-ui/react/dialog";
import { Input } from "@base-ui/react/input";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  Check,
  ChevronRight,
  CircleCheck,
  Cloud,
  LoaderCircle,
  Pencil,
  Plus,
  ShieldCheck,
  Trash2,
  Upload,
} from "lucide-react";
import { useRef, useState } from "react";
import {
  api,
  bucketsQuery,
  keys,
  snapshotsQuery,
  type Bucket,
  type RegisterBucket,
} from "../api/client";
import { CreateFromBucketDialog } from "./snapshot-actions";
import {
  Button,
  CopyText,
  EmptyState,
  ErrorNotice,
  Loading,
  Modal,
} from "./ui";

const idPattern = "[a-z0-9]([a-z0-9\\-]{0,46}[a-z0-9])?";

type Service = "aws" | "r2" | "other";
const services: {
  value: Service;
  label: string;
  region: string;
  endpoint: string;
}[] = [
  {
    value: "aws",
    label: "AWS S3",
    region: "ap-northeast-2",
    endpoint: "",
  },
  {
    value: "r2",
    label: "Cloudflare R2",
    region: "auto",
    endpoint: "https://<account-id>.r2.cloudflarestorage.com",
  },
  {
    value: "other",
    label: "S3 Compatible",
    region: "us-east-1",
    endpoint: "https://minio.example:9000",
  },
];
const serviceOf = (bucket?: Bucket): Service =>
  !bucket
    ? "other"
    : !bucket.endpoint
      ? "aws"
      : bucket.endpoint.includes(".r2.cloudflarestorage.com")
        ? "r2"
        : "other";

/** The prefix as farmd stores it: no leading slash, one trailing slash unless empty. */
const cleanPrefix = (prefix: string) => {
  const trimmed = prefix.trim().replace(/^\/+|\/+$/g, "");
  return trimmed ? `${trimmed}/` : "";
};
/** Where exports go by default: `s3://<name>/<prefix>`. */
export const bucketLocation = (bucket: Pick<Bucket, "name" | "prefix">) =>
  `s3://${bucket.name}/${bucket.prefix}`;

export function BucketsPanel() {
  const buckets = useQuery(bucketsQuery());
  const sorted = [...(buckets.data ?? [])].sort((a, b) =>
    a.id.localeCompare(b.id),
  );
  return (
    <section className="panel mt-6" aria-label="S3 버킷 목록">
      <div className="panel-toolbar">
        <div className="flex items-center gap-2.5">
          <h2>S3 버킷</h2>
          <span className="count-pill">{sorted.length}</span>
        </div>
        <BucketDialog />
      </div>
      {buckets.error && (
        <div className="p-5">
          <ErrorNotice
            error={buckets.error}
            retry={() => {
              void buckets.refetch();
            }}
          />
        </div>
      )}
      {buckets.error ? null : buckets.isPending ? (
        <Loading label="버킷을 불러오는 중" />
      ) : !sorted.length ? (
        <EmptyState
          icon={<Cloud size={24} />}
          title="등록된 S3 버킷이 없습니다"
          description="S3 Compatible 저장소를 등록하면 스냅샷을 올리고 다른 호스트에서 가져올 수 있습니다."
        />
      ) : (
        sorted.map((bucket) => <BucketRow bucket={bucket} key={bucket.id} />)
      )}
    </section>
  );
}

function BucketRow({ bucket }: { bucket: Bucket }) {
  const check = useMutation({ mutationFn: () => api.checkBucket(bucket.id) });
  const service = services.find((s) => s.value === serviceOf(bucket))!;
  return (
    <div className="bucket">
      <span className="bucket-name">
        <Cloud size={15} />
        {bucket.id}
      </span>
      <span className="bucket-kind">
        {service.label}
        {bucket.ca_pem && (
          <span className="chip" title="등록한 CA 인증서도 신뢰합니다">
            <ShieldCheck size={14} />
            사설 CA
          </span>
        )}
      </span>
      <div className="bucket-target">
        <div className="bucket-location">
          <code>{bucketLocation(bucket)}</code>
          <CopyText value={bucketLocation(bucket)} hideValue name="버킷 경로" />
        </div>
        <span className="bucket-endpoint">
          <span className="font-mono">
            {bucket.endpoint ?? `s3.${bucket.region}.amazonaws.com`}
          </span>
          {" · "}
          {bucket.region}
          {bucket.path_style ? " · path-style" : ""}
          {" · "}
          <span className="font-mono">{bucket.access_key_id}</span>
        </span>
        {check.isSuccess && (
          <span className="bucket-note">
            <CircleCheck size={14} />
            목록 조회, 쓰기, 삭제를 확인했습니다.
          </span>
        )}
        {check.error && (
          <p className="job-error">
            {check.error instanceof Error
              ? check.error.message
              : String(check.error)}
          </p>
        )}
      </div>
      <div className="bucket-actions">
        <CreateFromBucketDialog
          initialBucket={bucket.id}
          triggerVariant="ghost"
          triggerLabel="Farm 만들기"
        />
        <Button
          variant="ghost"
          disabled={check.isPending}
          onClick={() => check.mutate()}
        >
          {check.isPending ? (
            <LoaderCircle size={16} className="animate-spin" />
          ) : (
            <CircleCheck size={16} />
          )}
          연결 확인
        </Button>
        <BucketDialog bucket={bucket} />
        <DeleteBucketDialog bucket={bucket} />
      </div>
    </div>
  );
}

/** Registers a bucket, or edits `bucket`. The secret key is write-only. */
export function BucketDialog({ bucket }: { bucket?: Bucket }) {
  const client = useQueryClient();
  const editing = Boolean(bucket);
  const [open, setOpen] = useState(false);
  const [service, setService] = useState<Service>(serviceOf(bucket));
  const [id, setId] = useState("");
  const [endpoint, setEndpoint] = useState("");
  const [region, setRegion] = useState("");
  const [name, setName] = useState("");
  const [prefix, setPrefix] = useState("");
  const [accessKey, setAccessKey] = useState("");
  const [secret, setSecret] = useState("");
  const [pathStyle, setPathStyle] = useState(true);
  const [ca, setCa] = useState("");
  const [caFile, setCaFile] = useState("");
  const [advanced, setAdvanced] = useState(false);
  const [fileError, setFileError] = useState<Error>();
  const caInput = useRef<HTMLInputElement>(null);
  const preset = services.find((s) => s.value === service)!;
  const body = (): RegisterBucket => ({
    id,
    endpoint: service === "aws" ? null : endpoint.trim(),
    region: region.trim() || preset.region,
    name: name.trim(),
    prefix: prefix.trim(),
    path_style: service === "aws" ? false : pathStyle,
    ca_pem: service === "aws" ? null : ca.trim() || null,
    access_key_id: accessKey.trim(),
    ...(secret ? { secret_access_key: secret } : {}),
  });
  const test = useMutation({ mutationFn: () => api.testBucket(body()) });
  const save = useMutation({
    mutationFn: () =>
      editing ? api.updateBucket(body()) : api.registerBucket(body()),
    onSuccess: async () => {
      await client.invalidateQueries({ queryKey: keys.buckets });
      setOpen(false);
    },
  });
  const pending = save.isPending || test.isPending;
  const reset = () => {
    const kind = serviceOf(bucket);
    setService(kind);
    setId(bucket?.id ?? "");
    setEndpoint(bucket?.endpoint ?? "");
    setRegion(bucket?.region ?? "");
    setName(bucket?.name ?? "");
    setPrefix(bucket?.prefix ?? "farm/");
    setAccessKey(bucket?.access_key_id ?? "");
    setSecret("");
    setPathStyle(bucket?.path_style ?? true);
    setCa(bucket?.ca_pem ?? "");
    setCaFile("");
    // Show the advanced settings when they differ from the service's defaults.
    setAdvanced(
      Boolean(bucket?.ca_pem) ||
        (bucket !== undefined &&
          kind !== "aws" &&
          bucket.path_style !== (kind !== "r2")),
    );
    setFileError(undefined);
    test.reset();
    save.reset();
  };
  // A finished test describes the settings it ran with; editing invalidates it.
  const edited =
    <T,>(set: (value: T) => void) =>
    (value: T) => {
      set(value);
      test.reset();
    };
  return (
    <Dialog.Root
      open={open}
      onOpenChange={(next) => {
        if (pending) return;
        if (next) reset();
        setOpen(next);
      }}
    >
      <Dialog.Trigger
        render={
          editing ? (
            <Button variant="ghost" aria-label={`${bucket!.id} 버킷 수정`} />
          ) : (
            <Button variant="primary" />
          )
        }
      >
        {editing ? (
          <Pencil size={16} />
        ) : (
          <>
            <Plus size={16} />
            버킷 등록
          </>
        )}
      </Dialog.Trigger>
      <Modal
        title={editing ? "S3 버킷 수정" : "S3 버킷 등록"}
        description="스냅샷을 올리고 가져올 버킷입니다. Secret key는 암호화해서 저장합니다."
        className="bucket-modal"
        closeDisabled={pending}
      >
        <form
          onSubmit={(event) => {
            event.preventDefault();
            save.mutate();
          }}
        >
          <div className="modal-body space-y-6">
            <div
              className="segmented-control service-picker"
              role="radiogroup"
              aria-label="서비스"
            >
              {services.map((item) => (
                <button
                  key={item.value}
                  type="button"
                  role="radio"
                  aria-checked={service === item.value}
                  className={service === item.value ? "selected" : ""}
                  disabled={pending}
                  onClick={() => {
                    setService(item.value);
                    setRegion("");
                    setPathStyle(item.value !== "r2");
                    test.reset();
                  }}
                >
                  {item.label}
                </button>
              ))}
            </div>

            <section className="form-section">
              <h3>저장소</h3>
              <label className="field">
                <span>등록 ID</span>
                <Input
                  autoFocus={!editing}
                  value={id}
                  onValueChange={setId}
                  className="input font-mono"
                  required
                  maxLength={48}
                  pattern={idPattern}
                  placeholder="team-r2"
                  title="소문자, 숫자, 하이픈 · 최대 48자"
                  disabled={pending || editing}
                />
              </label>
              {service !== "aws" && (
                <label className="field">
                  <span>Endpoint</span>
                  <Input
                    value={endpoint}
                    onValueChange={edited(setEndpoint)}
                    className="input font-mono"
                    required
                    placeholder={preset.endpoint}
                    disabled={pending}
                  />
                </label>
              )}
              <div className="grid grid-cols-[3fr_2fr] items-end gap-3">
                <label className="field">
                  <span>버킷 이름</span>
                  <Input
                    value={name}
                    onValueChange={edited(setName)}
                    className="input font-mono"
                    required
                    placeholder="farm-snapshots"
                    disabled={pending}
                  />
                </label>
                <label className="field">
                  <span>
                    Region <span className="optional">선택</span>
                  </span>
                  <Input
                    value={region}
                    onValueChange={edited(setRegion)}
                    className="input font-mono"
                    placeholder={preset.region}
                    disabled={pending}
                  />
                </label>
              </div>
              <label className="field">
                <span>
                  Prefix <span className="optional">선택</span>
                </span>
                <Input
                  value={prefix}
                  onValueChange={edited(setPrefix)}
                  className="input font-mono"
                  placeholder="farm/"
                  disabled={pending}
                />
                <small>아카이브를 올리고 찾는 경로입니다.</small>
              </label>
            </section>

            <section className="form-section">
              <h3>인증</h3>
              <div className="grid grid-cols-2 items-end gap-3">
                <label className="field">
                  <span>Access key ID</span>
                  <Input
                    value={accessKey}
                    onValueChange={edited(setAccessKey)}
                    className="input font-mono"
                    required
                    autoComplete="off"
                    disabled={pending}
                  />
                </label>
                <label className="field">
                  <span>
                    Secret access key
                    {editing && <span className="optional">선택</span>}
                  </span>
                  <Input
                    type="password"
                    value={secret}
                    onValueChange={edited(setSecret)}
                    className="input font-mono"
                    required={!editing}
                    autoComplete="new-password"
                    placeholder={editing ? "비우면 기존 키 유지" : ""}
                    disabled={pending}
                  />
                </label>
              </div>
            </section>

            {service !== "aws" && (
              <details
                className="form-advanced"
                open={advanced}
                onToggle={(event) => setAdvanced(event.currentTarget.open)}
              >
                <summary>
                  <ChevronRight size={16} className="form-advanced-chevron" />
                  고급 설정
                  <span>
                    {[
                      pathStyle ? "Path-style" : "Virtual-host",
                      ca.trim() && "사설 CA",
                    ]
                      .filter(Boolean)
                      .join(" · ")}
                  </span>
                </summary>
                <div className="space-y-5 pt-4">
                  <label className="checkbox-row">
                    <Checkbox.Root
                      checked={pathStyle}
                      disabled={pending}
                      onCheckedChange={(next) => edited(setPathStyle)(next)}
                      className="checkbox"
                    >
                      <Checkbox.Indicator>
                        <Check size={14} strokeWidth={3} />
                      </Checkbox.Indicator>
                    </Checkbox.Root>
                    <span>
                      Path-style 주소
                      <small>
                        endpoint/버킷/키로 요청합니다. MinIO와 RustFS는
                        켜두세요.
                      </small>
                    </span>
                  </label>
                  <label className="field">
                    <span className="flex items-center justify-between gap-2">
                      <span>
                        사설 CA 인증서 <span className="optional">PEM</span>
                      </span>
                      <button
                        type="button"
                        className="text-link"
                        disabled={pending}
                        onClick={() => caInput.current?.click()}
                      >
                        <Upload size={15} />
                        파일 선택
                      </button>
                    </span>
                    <input
                      ref={caInput}
                      type="file"
                      accept=".pem,.crt,.cer"
                      className="sr-only"
                      aria-label="CA 인증서 파일"
                      disabled={pending}
                      onChange={(event) => {
                        const file = event.target.files?.[0];
                        event.target.value = "";
                        if (!file) return;
                        if (file.size > 65536) {
                          setFileError(
                            new Error(
                              "CA 인증서 파일은 64KB를 넘을 수 없습니다.",
                            ),
                          );
                          return;
                        }
                        void file.text().then(
                          (text) => {
                            edited(setCa)(text.replace(/\r\n/g, "\n"));
                            setCaFile(file.name);
                            setFileError(undefined);
                          },
                          () =>
                            setFileError(
                              new Error("CA 인증서 파일을 읽지 못했습니다."),
                            ),
                        );
                      }}
                    />
                    <textarea
                      className="input remote-key-input"
                      value={ca}
                      maxLength={65536}
                      disabled={pending}
                      spellCheck={false}
                      placeholder="-----BEGIN CERTIFICATE-----"
                      onChange={(e) => {
                        edited(setCa)(e.target.value);
                        setCaFile("");
                      }}
                    />
                    <small>
                      {caFile
                        ? `${caFile} 파일을 불러왔습니다.`
                        : "자체 서명 인증서를 쓰는 경우에만 넣으세요. 시스템 인증서와 함께 신뢰합니다."}
                    </small>
                  </label>
                </div>
              </details>
            )}

            {test.isSuccess && (
              <div className="info-notice mb-0">
                <CircleCheck size={17} className="shrink-0" />
                <span>
                  연결됐습니다.{" "}
                  <span className="font-mono">
                    {bucketLocation({
                      name: name.trim(),
                      prefix: cleanPrefix(prefix),
                    })}
                  </span>
                  에 쓰고 읽을 수 있습니다.
                </span>
              </div>
            )}
            {fileError && <ErrorNotice error={fileError} />}
            {test.error && <ErrorNotice error={test.error} />}
            {save.error && <ErrorNotice error={save.error} />}
          </div>
          <div className="modal-footer">
            <Button
              disabled={pending}
              onClick={(event) => {
                // Run the browser's required-field checks without submitting.
                const form = (event.currentTarget as HTMLElement).closest(
                  "form",
                );
                if (form && !form.reportValidity()) return;
                save.reset();
                test.mutate();
              }}
            >
              {test.isPending ? (
                <LoaderCircle size={16} className="animate-spin" />
              ) : (
                <CircleCheck size={16} />
              )}
              연결 확인
            </Button>
            <div className="flex gap-2">
              <Dialog.Close render={<Button disabled={pending} />}>
                취소
              </Dialog.Close>
              <Button type="submit" variant="primary" disabled={pending}>
                {save.isPending ? (
                  <LoaderCircle size={16} className="animate-spin" />
                ) : (
                  <Cloud size={16} />
                )}
                {editing ? "저장" : "등록"}
              </Button>
            </div>
          </div>
        </form>
      </Modal>
    </Dialog.Root>
  );
}

function DeleteBucketDialog({ bucket }: { bucket: Bucket }) {
  const client = useQueryClient();
  const [open, setOpen] = useState(false);
  const snapshots = useQuery(snapshotsQuery());
  const inUse = snapshots.data?.some((s) => s.transfer?.bucket === bucket.id);
  const mutation = useMutation({
    mutationFn: () => api.unregisterBucket(bucket.id),
    onSuccess: async () => {
      await client.invalidateQueries({ queryKey: keys.buckets });
      setOpen(false);
    },
  });
  return (
    <Dialog.Root
      open={open}
      onOpenChange={(next) => {
        if (mutation.isPending) return;
        if (next) mutation.reset();
        setOpen(next);
      }}
    >
      <Dialog.Trigger
        render={
          <Button variant="ghost" disabled={inUse} className="delete-trigger" />
        }
        aria-label={`${bucket.id} 버킷 등록 해제`}
      >
        <Trash2 size={16} />
      </Dialog.Trigger>
      <Modal
        title="버킷 등록을 해제할까요?"
        description="farm에 저장된 버킷 설정과 암호화된 키를 지웁니다. 버킷에 올린 아카이브는 그대로 남습니다."
        closeDisabled={mutation.isPending}
      >
        <div className="modal-body space-y-5">
          <div className="action-context">
            <Cloud size={20} />
            <div>
              <strong className="font-mono">{bucket.id}</strong>
              <p>{bucketLocation(bucket)}</p>
            </div>
          </div>
          {mutation.error && <ErrorNotice error={mutation.error} />}
        </div>
        <div className="modal-footer justify-end">
          <Dialog.Close render={<Button disabled={mutation.isPending} />}>
            취소
          </Dialog.Close>
          <Button
            variant="danger"
            disabled={mutation.isPending}
            onClick={() => mutation.mutate()}
          >
            {mutation.isPending ? (
              <LoaderCircle size={16} className="animate-spin" />
            ) : (
              <Trash2 size={16} />
            )}
            등록 해제
          </Button>
        </div>
      </Modal>
    </Dialog.Root>
  );
}
