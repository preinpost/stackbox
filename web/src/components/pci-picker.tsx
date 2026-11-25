import { Checkbox } from "@base-ui/react/checkbox";
import { Check } from "lucide-react";
import type { HostPciDevice, Stackbox } from "../api/client";

/** The stackbox on `hostId` that already passes each device through, by BDF. A device taken
 * this way is not offered again, whatever its binding says. */
export function occupiedPciOn(
  stackboxes: Stackbox[] | undefined,
  hostId: string | undefined,
) {
  const occupied = new Map<string, string>();
  if (!hostId) return occupied;
  for (const f of stackboxes ?? []) {
    if (f.host_id !== hostId || f.status === "deleted") continue;
    for (const bdf of f.plan?.pci_devices ?? []) occupied.set(bdf, f.id);
  }
  return occupied;
}

/** A device can be picked only when vfio-pci is ready and no other stackbox holds it. */
export const pciSelectable = (device: HostPciDevice, occupiedBy?: string) =>
  device.vfio_ready === true && !occupiedBy;

/** The checkbox list of a host's PCI devices for a compute VM, shared by every form that
 * attaches them. The parent owns the selection and filters it with `pciSelectable`. */
export function PciDevicePicker({
  devices,
  occupied,
  picked,
  onToggle,
  disabled = false,
}: {
  devices: HostPciDevice[];
  occupied: Map<string, string>;
  picked: string[];
  onToggle: (bdf: string, next: boolean) => void;
  disabled?: boolean;
}) {
  return (
    <div className="space-y-2">
      {devices.map((device) => {
        const occupiedBy = occupied.get(device.bdf);
        const reason = occupiedBy
          ? `사용 중(${occupiedBy})`
          : device.vfio_ready
            ? undefined
            : "준비 안 됨";
        return (
          <label
            key={device.bdf}
            className="checkbox-row"
            title={reason}
            aria-disabled={reason ? true : undefined}
          >
            <Checkbox.Root
              checked={picked.includes(device.bdf)}
              disabled={!!reason || disabled}
              onCheckedChange={(next) => onToggle(device.bdf, next)}
              className="checkbox"
            >
              <Checkbox.Indicator>
                <Check size={14} strokeWidth={3} />
              </Checkbox.Indicator>
            </Checkbox.Root>
            <span>
              <code className="font-mono">{device.bdf}</code>{" "}
              <small className="font-mono">
                {device.vendor_id}:{device.product_id} · 클래스 {device.class}
                {device.iommu_group != null &&
                  ` · 그룹 ${device.iommu_group}`}
              </small>
              <small>{reason ?? "vfio-pci 준비됨"}</small>
            </span>
          </label>
        );
      })}
    </div>
  );
}
