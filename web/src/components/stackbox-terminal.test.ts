import { expect, test } from "vitest";
import { parseControl, shellUrl } from "./stackbox-terminal";
import { shellTargets } from "./shell-targets";

test("builds a same-host shell URL that follows the page's scheme", () => {
  const page = (protocol: string) =>
    ({ protocol, host: "127.0.0.1:5180" }) as Location;
  expect(shellUrl(page("http:"), "first", 24, 80)).toBe(
    "ws://127.0.0.1:5180/api/v1/stackboxes/first/shell?rows=24&cols=80",
  );
  expect(shellUrl(page("https:"), "a b", 40, 120)).toBe(
    "wss://127.0.0.1:5180/api/v1/stackboxes/a%20b/shell?rows=40&cols=120",
  );
  expect(shellUrl(page("http:"), "mn", 24, 80, "compute-1")).toBe(
    "ws://127.0.0.1:5180/api/v1/stackboxes/mn/shell?rows=24&cols=80&node=compute-1",
  );
});

test("reads only well-formed control messages", () => {
  expect(parseControl('{"type":"exit"}')).toEqual({ type: "exit" });
  expect(parseControl('{"type":"error","message":"VM stopped"}')).toEqual({
    type: "error",
    message: "VM stopped",
  });
  expect(parseControl('{"type":"error"}')).toBeNull();
  expect(parseControl('{"type":"resize","rows":1,"cols":1}')).toBeNull();
  expect(parseControl("not json")).toBeNull();
});

test("lists the all-in-one VM alone, or multi-node VMs control first", () => {
  const vm = (name: string, role: string, index?: number) =>
    ({ name, role, index, status: "running", memory_mib: 1 }) as never;
  expect(shellTargets([vm("a", "all-in-one")]).map(({ key }) => key)).toEqual([
    "",
  ]);
  expect(
    shellTargets([
      vm("m-compute-2", "compute", 2),
      vm("m-compute-1", "compute", 1),
      vm("m-control", "control"),
    ]).map(({ key }) => key),
  ).toEqual(["control", "compute-1", "compute-2"]);
});
