import { expect, test } from "vitest";
import { parseControl, shellUrl } from "./farm-terminal";

test("builds a same-host shell URL that follows the page's scheme", () => {
  const page = (protocol: string) =>
    ({ protocol, host: "127.0.0.1:5180" }) as Location;
  expect(shellUrl(page("http:"), "first", 24, 80)).toBe(
    "ws://127.0.0.1:5180/api/v1/farms/first/shell?rows=24&cols=80",
  );
  expect(shellUrl(page("https:"), "a b", 40, 120)).toBe(
    "wss://127.0.0.1:5180/api/v1/farms/a%20b/shell?rows=40&cols=120",
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
