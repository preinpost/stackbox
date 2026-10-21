import { useQuery } from "@tanstack/react-query";
import {
  ArrowUpRight,
  Check,
  CircleAlert,
  Cpu,
  HardDrive,
  Image,
  Monitor,
  RefreshCw,
  Server,
  ShieldCheck,
  Terminal,
} from "lucide-react";
import { hostQuery } from "../api/client";
import { Button, ErrorNotice, Loading, PageHeading } from "../components/ui";
import { memory } from "../lib";

export function HostPage() {
  const host = useQuery(hostQuery());
  const h = host.data;
  const checks = h
    ? [
        {
          title: "microsandbox runtime",
          detail: "VM 생성과 실행을 담당합니다.",
          ready: h.msb_available,
          icon: Terminal,
        },
        {
          title: `${h.hypervisor.toUpperCase()} virtualization`,
          detail:
            h.os === "macos"
              ? "macOS 가상화 플랫폼 지원 여부"
              : "/dev/kvm 접근 가능 여부",
          ready: h.hypervisor_available,
          icon: ShieldCheck,
        },
        {
          title: "Golden image",
          detail: h.golden_image,
          ready: h.golden_image_available,
          icon: Image,
        },
      ]
    : [];
  return (
    <>
      <PageHeading
        eyebrow="HOSTS · LOCAL"
        title="로컬 · Coordinator"
        description="farm 데몬이 실행 중인 머신의 준비 상태와 리소스를 확인하세요."
        actions={
          <Button
            onClick={() => {
              void host.refetch();
            }}
            disabled={host.isFetching}
          >
            <RefreshCw
              size={15}
              className={host.isFetching ? "animate-spin" : ""}
            />
            새로고침
          </Button>
        }
      />
      {host.error && (
        <ErrorNotice
          error={host.error}
          retry={() => {
            void host.refetch();
          }}
        />
      )}
      {host.isPending ? (
        <Loading />
      ) : (
        h && (
          <>
            <section className="panel host-banner">
              <span className="host-banner-icon">
                <Monitor size={32} strokeWidth={1.4} />
              </span>
              <div>
                <p className="eyebrow">FARM HOST</p>
                <h2>
                  {h.os === "macos" ? "macOS" : h.os} <span>/ {h.arch}</span>
                </h2>
                <p>호스트의 가상화 기능으로 VM을 직접 실행합니다.</p>
              </div>
              <div
                className={`host-readiness ${checks.every((c) => c.ready) ? "ready" : ""}`}
              >
                {checks.every((c) => c.ready) ? (
                  <Check size={15} />
                ) : (
                  <CircleAlert size={15} />
                )}
                {checks.every((c) => c.ready)
                  ? "VM 생성 준비 완료"
                  : "설정 확인 필요"}
              </div>
            </section>
            <div className="host-checks">
              {checks.map(({ title, detail, ready, icon: Icon }) => (
                <div className="panel host-check" key={title}>
                  <div>
                    <Icon size={21} strokeWidth={1.6} />
                    <span className={ready ? "check-ok" : "check-missing"}>
                      {ready ? <Check size={14} /> : <CircleAlert size={14} />}
                      {ready ? "사용 가능" : "확인 필요"}
                    </span>
                  </div>
                  <h3>{title}</h3>
                  <p>{detail}</p>
                </div>
              ))}
            </div>
            <div className="host-resource-grid">
              <section className="panel host-resource">
                <div className="section-label">
                  <span className="small-icon">
                    <Cpu size={17} />
                  </span>
                  <h2>Memory</h2>
                </div>
                <div className="host-big-number">
                  {memory(h.memory_reserved_mib)}
                  <span>예약됨</span>
                </div>
                <div className="capacity-meter">
                  <span
                    style={{
                      width: `${Math.min(100, (h.memory_reserved_mib / Math.max(1, h.memory_budget_mib)) * 100)}%`,
                    }}
                  />
                </div>
                <dl className="host-dl">
                  <div>
                    <dt>게스트 예약 한도</dt>
                    <dd>{memory(h.memory_budget_mib)}</dd>
                  </div>
                  <div>
                    <dt>추가 예약 가능</dt>
                    <dd>
                      {memory(
                        Math.max(
                          0,
                          h.memory_budget_mib - h.memory_reserved_mib,
                        ),
                      )}
                    </dd>
                  </div>
                  <div>
                    <dt>호스트 전체 메모리</dt>
                    <dd>{memory(h.memory_total_mib)}</dd>
                  </div>
                  <div>
                    <dt>OS 가용 메모리 추정</dt>
                    <dd>{memory(h.memory_available_mib)}</dd>
                  </div>
                </dl>
                <p className="field-hint">
                  예약은 게스트에 할당할 메모리 기준이며, 호스트의 실제 사용량을
                  제한하지 않습니다.
                </p>
              </section>
              <section className="panel host-resource">
                <div className="section-label">
                  <span className="small-icon">
                    <HardDrive size={17} />
                  </span>
                  <h2>Storage & state</h2>
                </div>
                <div className="host-big-number">
                  {h.disk_available_mib != null
                    ? memory(h.disk_available_mib)
                    : "—"}
                  <span>여유 공간</span>
                </div>
                <dl className="host-dl">
                  <div>
                    <dt>VM 디스크</dt>
                    <dd>Sparse disks</dd>
                  </div>
                  <div>
                    <dt>상태 동기화</dt>
                    <dd>30초마다</dd>
                  </div>
                  <div>
                    <dt>CLI 작업</dt>
                    <dd>데몬 종료 후 사용</dd>
                  </div>
                </dl>
                <div className="state-path">
                  <Server size={14} />
                  <code>{h.state_dir}</code>
                </div>
                <p className="field-hint mt-4">
                  디스크 공간은 상태 디렉터리가 위치한 볼륨 기준입니다.
                </p>
              </section>
            </div>
            <section className="host-help">
              <Terminal size={20} />
              <div>
                <h3>호스트 준비가 필요한가요?</h3>
                <p>
                  microsandbox를 설치한 뒤 프로젝트 루트에서 golden image를
                  빌드하세요.
                </p>
                <code>task docker-build</code>
              </div>
              <a
                href="https://github.com/superradcompany/microsandbox"
                target="_blank"
                rel="noreferrer"
                className="text-link"
              >
                microsandbox
                <ArrowUpRight size={14} />
              </a>
            </section>
          </>
        )
      )}
    </>
  );
}
