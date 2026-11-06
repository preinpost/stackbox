import type { PlannedNode, ReleaseTemplates } from "../api/client";

/** How a release is offered: its templates here and in buckets, and whether it installs clean. */
export function releaseLabel(r: ReleaseTemplates) {
  const control = r.local_control + r.bucket_control;
  const compute = r.local_compute + r.bucket_compute;
  const buckets = r.bucket_control + r.bucket_compute;
  const parts = [
    ...(control + compute
      ? [`템플릿 control ${control} · compute ${compute}`]
      : []),
    ...(buckets ? [`버킷 ${buckets}`] : []),
    ...(control === 0 && r.installable ? ["클린 설치"] : []),
    // End of life: installed from its last kolla-ansible tag, and only where its older
    // images run (amd64), so an aarch64 coordinator offers it without the clean install.
    ...(r.eol ? ["EOL"] : []),
  ];
  return `OpenStack ${r.release}${parts.length ? ` · ${parts.join(" · ")}` : ""}`;
}

/** One VM of the preview: where it comes from and why. */
export function sourceLabel(node: PlannedNode) {
  switch (node.source) {
    case "template":
      return `템플릿 ${node.snapshot_id}`;
    case "bucket":
      return `버킷 ${node.bucket}의 ${node.key} → ${node.snapshot_id}로 가져와 사용`;
    default:
      return "클린 설치";
  }
}
