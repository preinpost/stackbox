import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  CircuitBoard,
  Download,
  RefreshCw,
  RotateCcw,
  Terminal,
  Zap,
} from "lucide-react";
import { useState } from "react";
import {
  api,
  gpuInstancesQuery,
  keys,
  type Farm,
  type FarmInstance,
  type GpuLoad,
  type GpuPattern,
  type GpuProfile,
} from "../api/client";
import {
  Button,
  EmptyState,
  ErrorNotice,
  Loading,
  SelectInput,
  StatusBadge,
} from "./ui";

/** What a GPU farm shows before anyone sets a load; the guest's own default. */
export const defaultGpuLoad: GpuLoad = {
  profile: "sim-8g",
  pattern: "sine",
  min: 10,
  max: 90,
  period_s: 60,
  memory_ratio: 30,
};

const profiles: { value: GpuProfile; label: string }[] = [
  { value: "sim-8g", label: "sim-8g · 8 GiB · 150 W" },
  { value: "sim-16g", label: "sim-16g · 16 GiB · 250 W" },
  { value: "sim-40g", label: "sim-40g · 40 GiB · 350 W" },
  { value: "sim-80g", label: "sim-80g · 80 GiB · 400 W" },
];

const patterns: { value: GpuPattern; label: string }[] = [
  { value: "constant", label: "일정" },
  { value: "sine", label: "사인" },
  { value: "triangle", label: "삼각" },
  { value: "square", label: "사각" },
  { value: "sawtooth", label: "톱니" },
  { value: "random", label: "랜덤" },
  { value: "spike", label: "스파이크" },
];

/** One period of utilization, exactly as farm samples it for the guest. */
export function gpuSamples(load: GpuLoad): number[] {
  const count = Math.max(1, Math.min(24, Math.floor(load.period_s)));
  const low = Math.min(load.min, load.max);
  const high = Math.max(load.min, load.max);
  const span = high - low;
  const mid = (low + high) / 2;
  const values = Array.from({ length: count }, (_, index) => {
    const t = index / count;
    switch (load.pattern) {
      case "constant":
        return mid;
      case "sine":
        return mid + (span / 2) * Math.sin(2 * Math.PI * t);
      case "triangle":
        return t < 0.5 ? low + span * (t * 2) : high - span * ((t - 0.5) * 2);
      case "square":
        return t < 0.5 ? high : low;
      case "sawtooth":
        return low + span * t;
      case "random": {
        // The same wrapping 64-bit mix as src/gpu.rs, over the same fields as the coordinator
        // (its validation refuses min > max, so the raw values match low and high there too).
        const mask = 0xffffffffffffffffn;
        let x =
          (BigInt(index) * 6364136223846793005n +
            BigInt(load.period_s) * 1442695040888963407n +
            (BigInt(load.min) << 32n) +
            BigInt(load.max)) &
          mask;
        x ^= x >> 33n;
        x = (x * 0xff51afd7ed558ccdn) & mask;
        x ^= x >> 33n;
        return low + (span * Number(x % 1000n)) / 1000;
      }
      case "spike":
        return count === 1 || index === Math.floor(count / 2) ? high : low;
    }
  });
  return values.map((value) => Math.max(0, Math.min(100, Math.round(value))));
}

function Sparkline({ values }: { values: number[] }) {
  const width = 220;
  const height = 48;
  const pad = 3;
  const step = values.length > 1 ? (width - pad * 2) / (values.length - 1) : 0;
  const points = values
    .map((value, index) => {
      const x = pad + index * step;
      const y = height - pad - (value / 100) * (height - pad * 2);
      return `${x.toFixed(1)},${y.toFixed(1)}`;
    })
    .join(" ");
  return (
    <svg
      className="gpu-spark"
      viewBox={`0 0 ${width} ${height}`}
      role="img"
      aria-label={`한 주기 사용률 ${values.join(", ")}`}
    >
      <line
        x1={pad}
        x2={width - pad}
        y1={height - pad}
        y2={height - pad}
        className="gpu-spark-axis"
      />
      <polyline points={points} />
    </svg>
  );
}

function gpuLabel(instance: FarmInstance) {
  const classes = Object.entries(instance.resources);
  if (!classes.length) return "GPU 리소스 없음 · 기본 1장";
  return classes.map(([name, count]) => `${name}×${count}`).join(" · ");
}

function GpuLoadRow({
  farmId,
  instance,
}: {
  farmId: string;
  instance: FarmInstance;
}) {
  const queryClient = useQueryClient();
  const [load, setLoad] = useState<GpuLoad>(instance.load ?? defaultGpuLoad);
  const [message, setMessage] = useState<string>();
  const invalid =
    load.min > load.max ||
    load.max > 100 ||
    load.period_s < 1 ||
    load.period_s > 3600 ||
    load.memory_ratio > 100;
  const samples = gpuSamples(load);
  const refresh = () => {
    void queryClient.invalidateQueries({
      queryKey: keys.gpuInstances(farmId),
    });
  };
  const apply = useMutation({
    mutationFn: () => api.setGpuLoad(farmId, instance.id, load),
    onSuccess: (view) => {
      setLoad(view.load);
      setMessage("적용했습니다. 게스트 반영까지 최대 약 15초 걸립니다.");
      refresh();
    },
    onError: (error: Error) => setMessage(error.message),
  });
  const reset = useMutation({
    mutationFn: () => api.clearGpuLoad(farmId, instance.id),
    onSuccess: (view) => {
      setLoad(view.load);
      setMessage(
        "기본값으로 되돌렸습니다. 게스트 반영까지 최대 약 15초 걸립니다.",
      );
      refresh();
    },
    onError: (error: Error) => setMessage(error.message),
  });
  const update = <K extends keyof GpuLoad>(key: K, value: GpuLoad[K]) =>
    setLoad((current) => ({ ...current, [key]: value }));
  const updateNumber = (
    key: "min" | "max" | "period_s" | "memory_ratio",
    value: string,
  ) => {
    const number = Number(value);
    update(key, Number.isFinite(number) ? number : 0);
  };
  return (
    <article className="gpu-row">
      <header className="gpu-row-head">
        <div>
          <strong>{instance.name}</strong>
          <StatusBadge status={instance.status} />
          {instance.applied && <span className="tag">적용됨</span>}
        </div>
        <span className="field-hint">
          {instance.flavor} · {gpuLabel(instance)}
        </span>
      </header>
      <div className="gpu-editor">
        <label className="field">
          <span className="field-label">프로파일</span>
          <SelectInput
            value={load.profile}
            onChange={(value) => update("profile", value)}
            items={profiles}
            label={`${instance.name} 프로파일`}
          />
        </label>
        <label className="field">
          <span className="field-label">패턴</span>
          <SelectInput
            value={load.pattern}
            onChange={(value) => update("pattern", value)}
            items={patterns}
            label={`${instance.name} 패턴`}
          />
        </label>
        <label className="field">
          <span className="field-label">최소 사용률 (%)</span>
          <input
            className="input"
            type="number"
            min={0}
            max={100}
            value={load.min}
            onChange={(event) => updateNumber("min", event.target.value)}
          />
        </label>
        <label className="field">
          <span className="field-label">최대 사용률 (%)</span>
          <input
            className="input"
            type="number"
            min={0}
            max={100}
            value={load.max}
            onChange={(event) => updateNumber("max", event.target.value)}
          />
        </label>
        <label className="field">
          <span className="field-label">주기 (초)</span>
          <input
            className="input"
            type="number"
            min={1}
            max={3600}
            value={load.period_s}
            onChange={(event) => updateNumber("period_s", event.target.value)}
          />
        </label>
        <label className="field">
          <span className="field-label">메모리 사용 비율 (%)</span>
          <input
            className="input"
            type="number"
            min={0}
            max={100}
            value={load.memory_ratio}
            onChange={(event) =>
              updateNumber("memory_ratio", event.target.value)
            }
          />
        </label>
      </div>
      <div className="gpu-preview">
        <Sparkline values={samples} />
        <div>
          <strong>
            {load.min}–{load.max}% · {load.period_s}초 · 메모리{" "}
            {load.memory_ratio}%
          </strong>
          <span className="field-hint">
            게스트는 현재 초에 해당하는 표본 하나를 읽고
            온도·전력·팬·클럭·pstate를 정수 연산으로 계산합니다.
          </span>
        </div>
      </div>
      {invalid && (
        <p className="field-error" role="alert">
          최소가 최대보다 크거나 범위를 벗어났습니다. 0–100%, 주기 1–3600초로
          맞춰 주세요.
        </p>
      )}
      <div className="gpu-actions">
        <Button
          variant="primary"
          onClick={() => apply.mutate()}
          disabled={invalid || apply.isPending}
        >
          <Zap size={16} />
          {apply.isPending ? "적용 중" : "적용"}
        </Button>
        <Button
          variant="ghost"
          onClick={() => reset.mutate()}
          disabled={(!instance.applied && !instance.load) || reset.isPending}
        >
          <RotateCcw size={16} />
          {reset.isPending ? "되돌리는 중" : "기본값"}
        </Button>
        {message && (
          <span className="field-hint" role="status">
            {message}
          </span>
        )}
      </div>
    </article>
  );
}

/** The GPU tab of a farm: instances, their load, a preview, and install help. */
export function GpuLoadPanel({ farm }: { farm: Farm }) {
  const ready = farm.status === "ready" && !farm.active_job;
  const query = useQuery({ ...gpuInstancesQuery(farm.id), enabled: ready });
  const downloads = [
    {
      href: `/api/v1/farms/${encodeURIComponent(farm.id)}/gpu/guest-script`,
      name: "nvidia-smi.sh",
      label: "가짜 nvidia-smi",
    },
    {
      href: `/api/v1/farms/${encodeURIComponent(farm.id)}/gpu/user-data`,
      name: "farm-gpu-user-data.sh",
      label: "user-data 설치 스크립트",
    },
  ];
  return (
    <section className="panel gpu-panel">
      <div className="panel-toolbar">
        <h2 className="flex items-center gap-2">
          <CircuitBoard size={16} />
          가짜 nvidia-smi 부하
        </h2>
        <span className="chip mono">
          {farm.plan?.gpu?.mode === "mtty" ? "mtty" : "simulated"} ·{" "}
          {farm.plan?.gpu?.count ?? 0} GPU
        </span>
      </div>
      <div className="panel-body gpu-panel-body">
        <p className="field-hint">
          farm이 부하 표를 인스턴스 메타데이터에 넣고, 게스트의 가짜{" "}
          <code>nvidia-smi</code>가 그 값을 읽습니다. 실제 GPU 연산은 하지
          않으며, Nova 메타데이터 캐시 때문에 반영까지 최대 약 15초 걸립니다.
        </p>
        <div className="gpu-install">
          <div>
            <strong>
              <Download size={15} /> 게스트 설치
            </strong>
            <p className="field-hint">
              cloud-init 이미지는 이 farm의 vendordata로 자동 설치됩니다.
              CirrOS처럼 vendordata를 적용하지 않는 이미지는 user-data 설치
              스크립트를 인스턴스에 넘기세요. 실제 GPU(PCI vendor 10de)가 보이면
              설치는 스스로 건너뜁니다.
            </p>
          </div>
          <div className="gpu-downloads">
            {downloads.map((item) => (
              <a
                className="btn btn-default"
                href={item.href}
                download={item.name}
                key={item.href}
              >
                <Terminal size={16} />
                {item.label}
              </a>
            ))}
          </div>
        </div>
        <p className="field-hint">
          Telegraf <code>inputs.nvidia_smi</code>는 기본 경로{" "}
          <code>/usr/bin/nvidia-smi</code>에서 <code>-q -x</code>를 실행합니다.
          별도 설정 없이 같은 measurement가 나옵니다.
        </p>
        {!ready ? (
          <div className="endpoint-note">
            <RefreshCw size={15} /> 배포가 끝나면 인스턴스 목록과 부하 편집기가
            나타납니다.
          </div>
        ) : query.isPending ? (
          <Loading label="인스턴스 목록을 불러오는 중" />
        ) : query.error ? (
          <ErrorNotice
            error={query.error}
            retry={() => {
              void query.refetch();
            }}
          />
        ) : !query.data.length ? (
          <EmptyState
            icon={<CircuitBoard size={24} />}
            title="인스턴스가 없습니다"
            description="GPU flavor로 인스턴스를 만들면 여기에서 부하를 조절할 수 있습니다."
          />
        ) : (
          <div className="gpu-list">
            {query.data.map((instance) => (
              <GpuLoadRow
                farmId={farm.id}
                instance={instance}
                key={`${instance.id}:${JSON.stringify(instance.load ?? null)}`}
              />
            ))}
          </div>
        )}
      </div>
    </section>
  );
}
