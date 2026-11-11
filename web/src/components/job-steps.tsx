import { useEffect, useState } from "react";
import { Check, CircleAlert, LoaderCircle } from "lucide-react";
import { activeJob, type Job } from "../api/client";
import { duration } from "../lib";

type Step = NonNullable<Job["steps"]>[number];

/** The numbered step a running job is in, such as a freeze's capture of one VM. */
export function currentStep(job: Job | undefined): Step | undefined {
  if (!job || !activeJob(job)) return undefined;
  const last = job.steps?.at(-1);
  return last && last.finished_at == null ? last : undefined;
}

export const stepLabel = (step: Step) =>
  `${step.index}/${step.total} ${step.step}`;

/** Seconds since the epoch, ticking while `live`. */
export function useNow(live: boolean) {
  const [now, setNow] = useState(() => Date.now() / 1000);
  useEffect(() => {
    if (!live) return;
    setNow(Date.now() / 1000);
    const tick = setInterval(() => setNow(Date.now() / 1000), 1000);
    return () => clearInterval(tick);
  }, [live]);
  return now;
}

/** The running job's current step with a spinner, i/N and the time spent in it. */
export function StepProgress({ job }: { job: Job | undefined }) {
  const step = currentStep(job);
  const now = useNow(Boolean(step));
  if (!step) return null;
  return (
    <span
      className="phase-step"
      title={stepLabel(step)}
      aria-label={`${step.total}단계 중 ${step.index}단계: ${step.step}`}
    >
      <LoaderCircle size={11} className="animate-spin" />
      <span className="phase-step-text">
        {stepLabel(step)} · {duration(now - step.started_at)}
      </span>
    </span>
  );
}

/** Every step a job went through, with how long each took. */
export function StepList({ job, now }: { job: Job; now: number }) {
  if (!job.steps?.length) return null;
  return (
    <ol className="job-steps" aria-label="작업 단계">
      {job.steps.map((step) => {
        const running = step.finished_at == null && activeJob(job);
        return (
          <li
            key={`${step.index}-${step.started_at}`}
            className={step.failed ? "failed" : running ? "running" : "done"}
          >
            {step.failed ? (
              <CircleAlert size={13} />
            ) : running ? (
              <LoaderCircle size={13} className="animate-spin" />
            ) : (
              <Check size={13} />
            )}
            <span className="font-mono">
              {step.index}/{step.total}
            </span>
            <span className="job-step-text">{step.step}</span>
            <span className="muted">
              {duration((step.finished_at ?? now) - step.started_at)}
            </span>
          </li>
        );
      })}
    </ol>
  );
}
