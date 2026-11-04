import { Link } from "@tanstack/react-router";
import {
  ArrowRight,
  CopyPlus,
  Cpu,
  Play,
  Plus,
  ShieldCheck,
  Snowflake,
  Trash2,
} from "lucide-react";
import type { Job } from "../api/client";
import { StatusBadge } from "../components/ui";
import { dateTime, jobKindLabels, relativeTime } from "../lib";
import { JobsMonitor } from "./jobs-monitor";

export const jobLabel = (kind: Job["kind"]) => jobKindLabels[kind];
const jobIcons = {
  create: Plus,
  deploy: Play,
  expand: Cpu,
  delete: Trash2,
  snapshot: Snowflake,
  restore: CopyPlus,
  settle: ShieldCheck,
};
export function JobsPage() {
  return <JobsMonitor />;
}
export function JobRow({ job }: { job: Job }) {
  const Icon = jobIcons[job.kind];
  return (
    <div className="job-row">
      <span className={`job-kind-icon ${job.status}`}>
        <Icon size={16} />
      </span>
      <div className="job-row-main">
        <div className="flex flex-wrap items-center gap-2.5">
          <Link
            to="/farms/$farmId"
            params={{ farmId: job.farm_id }}
            className="font-medium hover:underline"
          >
            {job.farm_id}
          </Link>
          <span className="muted text-xs">
            {jobLabel(job.kind)}
            {job.snapshot_id && ` · ${job.snapshot_id}`}
          </span>
          <StatusBadge status={job.status} />
        </div>
        <div className="job-meta">
          <span className="font-mono">{job.id.slice(0, 8)}</span>
          {job.phase && (
            <>
              <span>·</span>
              <span>{job.phase}</span>
            </>
          )}
          <span>·</span>
          <span title={dateTime(job.created_at)}>
            {relativeTime(job.created_at)}
          </span>
        </div>
        {job.error && <p className="job-error">{job.error}</p>}
      </div>
      <Link
        to="/farms/$farmId"
        params={{ farmId: job.farm_id }}
        className="icon-button shrink-0"
        aria-label={`${job.farm_id} 작업 상세`}
      >
        <ArrowRight size={16} />
      </Link>
    </div>
  );
}
