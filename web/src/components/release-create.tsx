import type { PlannedNode, ReleaseTemplates } from "../api/client";

const archNames: Record<string, string> = { x86_64: "amd64", aarch64: "arm64" };
/** An architecture by its image name, `amd64` or `arm64`. */
export const archName = (arch: string) => archNames[arch] ?? arch;

/** Whether a host of `arch` can run the release's images; unknown when either is missing. */
export function runsOn(r: ReleaseTemplates, arch?: string | null) {
  return !arch || !r.arches?.length || r.arches.includes(arch);
}

/** How a release is offered: its templates here and in buckets, whether it installs clean,
 * and the architectures its images run on, saying so when the chosen host cannot. */
export function releaseLabel(r: ReleaseTemplates, arch?: string | null) {
  const control = r.local_control + r.bucket_control;
  const compute = r.local_compute + r.bucket_compute;
  const buckets = r.bucket_control + r.bucket_compute;
  // The code name says which series a release is; a series without a published name shows
  // its number alone.
  const codename = r.codename ? ` (${r.codename})` : "";
  const parts = [
    ...(control + compute
      ? [`템플릿 control ${control} · compute ${compute}`]
      : []),
    ...(buckets ? [`버킷 ${buckets}`] : []),
    ...(control === 0 && r.installable ? ["클린 설치"] : []),
    // End of life: installed from its last kolla-ansible tag, and only where its older
    // images run (amd64), so an aarch64 coordinator offers it without the clean install.
    ...(r.eol ? ["EOL"] : []),
    ...(r.arches?.length ? [r.arches.map(archName).join("/")] : []),
    // `runsOn` already said no, which means `arch` names one.
    ...(arch && !runsOn(r, arch) ? [`${archName(arch)} 호스트 불가`] : []),
  ];
  return `OpenStack ${r.release}${codename}${parts.length ? ` · ${parts.join(" · ")}` : ""}`;
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
