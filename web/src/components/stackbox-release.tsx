import type { Stackbox } from "../api/client";

/** `OpenStack 2022.2 (Zed)`, or nothing for a stackbox recorded before plans kept a release. The
 * code name says which series a release is; one without a published name is shown by its
 * number alone. */
export const stackboxReleaseLabel = (stackbox: Stackbox) => {
  const release = stackbox.openstack?.release ?? stackbox.plan?.release;
  if (!release) return undefined;
  const codename = stackbox.openstack?.codename;
  return `OpenStack ${release}${codename ? ` (${codename})` : ""}`;
};

/** Why a stackbox's release is outside the supported range: `EOL` for a series whose kolla-ansible
 * branch is gone, `지원 범위 밖` for one with no published images at all. Absent while the
 * release is supported, and while the coordinator does not say (one that predates `openstack`). */
export const stackboxReleaseNote = (stackbox: Stackbox) => {
  if (!stackbox.openstack || stackbox.openstack.supported) return undefined;
  return stackbox.openstack.eol ? "EOL" : "지원 범위 밖";
};

/** The OpenStack release a stackbox installs, with the reason this coordinator no longer supports
 * it beside the version it belongs to. Renders nothing when the stackbox kept no release; callers
 * name that case themselves, as `호스트 없음` does for a stackbox without a host. */
export function StackboxRelease({ stackbox }: { stackbox: Stackbox }) {
  const label = stackboxReleaseLabel(stackbox);
  const note = stackboxReleaseNote(stackbox);
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
