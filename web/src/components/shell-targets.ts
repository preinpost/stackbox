import type { Farm } from "../api/client";

type FarmNode = Farm["nodes"][number];
/** A VM the shell can open: `key` is its multi-node suffix, empty for the farm's default VM. */
export type ShellTarget = { key: string; node: FarmNode };

/** Shell-able VMs, control first: the all-in-one VM alone, or each multi-node VM by suffix. */
export function shellTargets(nodes: FarmNode[]): ShellTarget[] {
  const aio = nodes.find((node) => node.role === "all-in-one");
  if (aio) return [{ key: "", node: aio }];
  const suffix = (node: FarmNode) =>
    node.role === "control"
      ? "control"
      : node.role === "compute" && node.index
        ? `compute-${node.index}`
        : null;
  return nodes
    .flatMap((node) => {
      const key = suffix(node);
      return key ? [{ key, node }] : [];
    })
    .sort(
      (a, b) =>
        Number(b.key === "control") - Number(a.key === "control") ||
        (a.node.index ?? 0) - (b.node.index ?? 0),
    );
}
