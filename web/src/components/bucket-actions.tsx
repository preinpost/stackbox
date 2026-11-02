import { Checkbox } from "@base-ui/react/checkbox";
import { Dialog } from "@base-ui/react/dialog";
import { Input } from "@base-ui/react/input";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  Check,
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
import {
  Button,
  CopyText,
  EmptyState,
  ErrorNotice,
  Loading,
  Modal,
  SelectInput,
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
    label: "MinIO · RustFS · 기타 S3 호환",
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
    <div className="job-row">
      <span className="job-kind-icon">
        <Cloud size={16} />
      </span>
      <div className="job-row-main">
        <div className="flex flex-wrap items-center gap-2.5">
          <strong className="font-mono text-stone-800">{bucket.id}</strong>
          <span className="snapshot-host">{service.label}</span>
          {bucket.ca_pem && (
            <span
              className="snapshot-host"
              title="등록한 CA 인증서도 신뢰합니다"
            >
              <ShieldCheck size={12} />
              사설 CA
            </span>
          )}
        </div>
        <div className="job-meta flex-wrap">
          <CopyText value={bucketLocation(bucket)} />
          <span>·</span>
          <span className="font-mono">
            {bucket.endpoint ?? `s3.${bucket.region}.amazonaws.com`}
          </span>
          <span>·</span>
          <span>{bucket.region}</span>
          {bucket.path_style && (
            <>
              <span>·</span>
              <span>path-style</span>
            </>
          )}
          <span>·</span>
          <span className="font-mono">{bucket.access_key_id}</span>
        </div>
        {check.isSuccess && (
          <p className="mt-2 flex items-center gap-1.5 text-xs text-emerald-700">
            <CircleCheck size={13} />
            목록 조회, 쓰기, 삭제를 확인했습니다.
          </p>
        )}
        {check.error && (
          <p className="job-error">
            {check.error instanceof Error
              ? check.error.message
              : String(check.error)}
          </p>
        )}
      </div>
      <div className="flex shrink-0 flex-wrap items-center gap-2">
        <Button disabled={check.isPending} onClick={() => check.mutate()}>
          {check.isPending ? (
            <LoaderCircle size={14} className="animate-spin" />
          ) : (
            <CircleCheck size={14} />
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
    setService(serviceOf(bucket));
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
          <Pencil size={15} />
        ) : (
          <>
            <Plus size={14} />
            버킷 등록
          </>
        )}
      </Dialog.Trigger>
      <Modal
        title={editing ? "S3 버킷 수정" : "S3 버킷 등록"}
        description="스냅샷 아카이브를 올리고 가져올 S3 호환 버킷입니다. Secret key는 farmd가 암호화해서 저장하고 다시 보여주지 않습니다."
        className="remote-register-modal"
        closeDisabled={pending}
      >
        <form
          onSubmit={(event) => {
            event.preventDefault();
            save.mutate();
          }}
        >
          <div className="modal-body space-y-5">
            <div className="grid grid-cols-2 gap-3">
              <label className="field">
                <span>버킷 ID</span>
                <Input
                  autoFocus={!editing}
                  value={id}
                  onValueChange={setId}
                  className="input font-mono"
                  required
                  maxLength={48}
                  pattern={idPattern}
                  placeholder="team-r2"
                  disabled={pending || editing}
                />
                <small>farm에서 부를 이름</small>
              </label>
              <div className="field">
                <span>서비스</span>
                <SelectInput
                  label="서비스"
                  value={service}
                  disabled={pending}
                  items={services.map((s) => ({
                    value: s.value,
                    label: s.label,
                  }))}
                  onChange={(value) => {
                    setService(value);
                    setRegion("");
                    setPathStyle(value !== "r2");
                    test.reset();
                  }}
                />
              </div>
            </div>
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
                <small>
                  scheme과 호스트, 포트만 적습니다. 사내 저장소는 http://도
                  됩니다.
                </small>
              </label>
            )}
            <div className="grid grid-cols-2 gap-3">
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
              <small>
                아카이브를 이 경로 아래에 올리고, 가져오기는 이 경로 아래의 .msb
                파일을 보여줍니다.
              </small>
            </label>
            <div className="grid grid-cols-2 gap-3">
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
                  Secret access key{" "}
                  {editing && <span className="optional">선택</span>}
                </span>
                <Input
                  type="password"
                  value={secret}
                  onValueChange={edited(setSecret)}
                  className="input font-mono"
                  required={!editing}
                  autoComplete="new-password"
                  placeholder={editing ? "비워 두면 저장된 키 유지" : ""}
                  disabled={pending}
                />
              </label>
            </div>
            {service !== "aws" && (
              <>
                <label className="checkbox-row">
                  <Checkbox.Root
                    checked={pathStyle}
                    disabled={pending}
                    onCheckedChange={(next) => edited(setPathStyle)(next)}
                    className="checkbox"
                  >
                    <Checkbox.Indicator>
                      <Check size={12} strokeWidth={3} />
                    </Checkbox.Indicator>
                  </Checkbox.Root>
                  <span>
                    Path-style 주소
                    <small>
                      endpoint/버킷/키 형태로 요청합니다. MinIO와 RustFS는 보통
                      켜고, R2는 어느 쪽이든 됩니다.
                    </small>
                  </span>
                </label>
                <label className="field">
                  <span className="flex items-center justify-between gap-2">
                    <span>
                      사설 CA 인증서 <span className="optional">선택</span>
                    </span>
                    <button
                      type="button"
                      className="text-link"
                      disabled={pending}
                      onClick={() => caInput.current?.click()}
                    >
                      <Upload size={13} />
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
                      : "자체 서명·사내 CA로 TLS를 쓰는 경우 PEM을 붙여넣으세요. 시스템 인증서와 함께 신뢰합니다."}
                  </small>
                </label>
              </>
            )}
            {test.isSuccess && (
              <div className="info-notice">
                <CircleCheck size={17} className="shrink-0" />
                <span>
                  연결을 확인했습니다.{" "}
                  {bucketLocation({
                    name: name.trim(),
                    prefix: cleanPrefix(prefix),
                  })}{" "}
                  아래에 목록 조회, 쓰기, 삭제가 됩니다.
                </span>
              </div>
            )}
            {fileError && <ErrorNotice error={fileError} />}
            {test.error && <ErrorNotice error={test.error} />}
            {save.error && <ErrorNotice error={save.error} />}
          </div>
          <div className="modal-footer justify-between">
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
                <LoaderCircle size={15} className="animate-spin" />
              ) : (
                <CircleCheck size={15} />
              )}
              연결 확인
            </Button>
            <div className="flex gap-2">
              <Dialog.Close render={<Button disabled={pending} />}>
                취소
              </Dialog.Close>
              <Button type="submit" variant="primary" disabled={pending}>
                {save.isPending ? (
                  <LoaderCircle size={15} className="animate-spin" />
                ) : (
                  <Cloud size={15} />
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
              <LoaderCircle size={15} className="animate-spin" />
            ) : (
              <Trash2 size={15} />
            )}
            등록 해제
          </Button>
        </div>
      </Modal>
    </Dialog.Root>
  );
}
