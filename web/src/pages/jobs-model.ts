import { activeJob, type HostJob, type Job } from "../api/client";
import { currentStep, stepLabel } from "../components/job-steps";
import { duration, jobKindLabels, phases } from "../lib";

export type Activity =
  { scope: "stackbox"; job: Job } | { scope: "host"; job: HostJob };
export type ActivityRow = { activity: Activity; previous: ActivityRow[] };
export type HistoryFilters = {
  search: string;
  status: string;
  kind: string;
  days: string;
};
export const subject = (a: Activity) =>
  a.scope === "stackbox" ? a.job.stackbox_id : a.job.host_id;
export const startedAt = (a: Activity) =>
  a.scope === "stackbox"
    ? (a.job.started_at ?? a.job.created_at)
    : a.job.created_at;
export const activityLabel = (a: Activity) =>
  a.scope === "stackbox"
    ? jobKindLabels[a.job.kind]
    : {
        check: "SSH 점검",
        prepare: "환경 준비",
        install_agent: "Stackbox 실행기 업데이트",
        purge: "호스트 VM 정리",
      }[a.job.kind];
export function activityStep(a: Activity) {
  if (a.job.status === "queued") return "실행 대기";
  if (a.scope === "host")
    return activeJob(a.job)
      ? (a.job.events.at(-1)?.message ?? "작업 시작 중")
      : "—";
  // An assemble job is as far as its running step.
  const steps = a.job.assembly ?? [];
  const running = steps.findIndex((step) => activeJob(step));
  if (steps.length && activeJob(a.job))
    return running < 0
      ? "다음 단계 준비 중"
      : `${running + 1}/${steps.length}단계 · ${steps[running].description}`;
  const step = currentStep(a.job);
  if (step) return stepLabel(step);
  return (
    phases.find((p) => p.id === a.job.phase)?.description ??
    (activeJob(a.job) ? activityLabel(a) : "—")
  );
}
export function failureSummary(a: Activity) {
  if (!["failed", "interrupted"].includes(a.job.status)) return null;
  if (a.job.status === "interrupted") return "작업이 중단되었습니다";
  return `${a.scope === "stackbox" && a.job.phase ? (phases.find((p) => p.id === a.job.phase)?.label ?? a.job.phase) : activityLabel(a)} 실패`;
}
export function elapsed(a: Activity, now: number) {
  if (a.job.status === "queued") return "대기 중";
  if (!activeJob(a.job) && a.job.finished_at == null) return "—";
  return duration((a.job.finished_at ?? now) - startedAt(a));
}
export function historyRows(
  activities: Activity[],
  filters: HistoryFilters,
  grouped: boolean,
  now: number,
): ActivityRow[] {
  const query = filters.search.trim().toLowerCase();
  const filtered = activities
    .filter(
      (a) =>
        !activeJob(a.job) &&
        (filters.status === "all" || a.job.status === filters.status) &&
        (filters.kind === "all" || a.job.kind === filters.kind) &&
        (filters.days === "all" ||
          a.job.created_at >= now - Number(filters.days) * 86400) &&
        `${subject(a)} ${a.job.id}`.toLowerCase().includes(query),
    )
    .sort(
      (a, b) =>
        b.job.created_at - a.job.created_at || b.job.id.localeCompare(a.job.id),
    );
  const rows: ActivityRow[] = [];
  const targets = new Map<string, ActivityRow>();
  for (const activity of filtered) {
    const key = `${activity.scope}:${subject(activity)}`;
    const previous = grouped ? targets.get(key) : undefined;
    const row = { activity, previous: [] };
    if (previous) previous.previous.push(row);
    else {
      rows.push(row);
      targets.set(key, row);
    }
  }
  return rows;
}
