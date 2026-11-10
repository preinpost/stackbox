import type { Farm } from "../api/client";

/** `OpenStack 2022.2 (zed)`, or nothing for a farm recorded before plans kept a release. The
 * code name says which series a release is; the `year.release` names carry it already, so it is
 * added only when it differs. */
export const farmReleaseLabel = (farm: Farm) => {
  const release = farm.openstack?.release ?? farm.plan?.release;
  if (!release) return undefined;
  const series = farm.openstack?.series;
  return `OpenStack ${release}${series && series !== release ? ` (${series})` : ""}`;
};

/** Why a farm's release is outside the supported range: `EOL` for a series whose kolla-ansible
 * branch is gone, `지원 범위 밖` for one with no published images at all. Absent while the
 * release is supported, and while the coordinator does not say (one that predates `openstack`). */
export const farmReleaseNote = (farm: Farm) => {
  if (!farm.openstack || farm.openstack.supported) return undefined;
  return farm.openstack.eol ? "EOL" : "지원 범위 밖";
};

/** The OpenStack release a farm installs, with the reason this coordinator no longer supports
 * it beside the version it belongs to. Renders nothing when the farm kept no release; callers
 * name that case themselves, as `호스트 없음` does for a farm without a host. */
export function FarmRelease({ farm }: { farm: Farm }) {
  const label = farmReleaseLabel(farm);
  const note = farmReleaseNote(farm);
  if (!label) return null;
  return (
    <>
      {label}
      {note && (
        <span
          className="release-flag"
          title="지원하는 가장 오래된 릴리스보다 이전 버전입니다"
        >
          {note}
        </span>
      )}
    </>
  );
}
