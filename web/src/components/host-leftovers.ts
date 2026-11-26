import type { HostJob } from "../api/client";

/** The purge job event that names what stayed on the host, written by the coordinator. */
const PREFIX = "Left on the host: ";
const SEPARATOR = " | ";
const BY_HAND = ". To remove the directory by hand, run on the host: ";

export type HostLeftovers = {
  /** What is still on the host, each with its reason: kept on purpose, failed, or unknown. */
  items: string[];
  /** The command that removes the stackbox directory, on an SSH host only. */
  byHand?: string;
};

/** What a purge job left on the host, or `undefined` when it left nothing (or did not ask). */
export function jobLeftovers(job: HostJob): HostLeftovers | undefined {
  let message: string | undefined;
  for (const event of job.events) {
    if (event.message.startsWith(PREFIX)) message = event.message;
  }
  if (message === undefined) return undefined;
  const body = message.slice(PREFIX.length);
  const at = body.indexOf(BY_HAND);
  if (at < 0) return { items: body.split(SEPARATOR) };
  return {
    items: body.slice(0, at).split(SEPARATOR),
    byHand: body.slice(at + BY_HAND.length),
  };
}
