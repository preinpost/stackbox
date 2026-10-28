import { Dialog } from "@base-ui/react/dialog";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, test, vi } from "vitest";
import {
  RegisterAgentHostDialog,
  RegisterHostForm,
  RemoteHostsPage,
} from "./remote-hosts";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});
const fingerprint = `SHA256:${"A".repeat(43)}`;

test("requires explicit fingerprint verification, clears it on address changes and keeps credentials out of Query caches", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  let registrations = 0;
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string, options?: RequestInit) => {
      if (path.endsWith("/fingerprint"))
        return Response.json({ fingerprint, algorithm: "ssh-ed25519" });
      registrations++;
      const body = JSON.parse(String(options?.body));
      expect(body.credentials).toEqual({
        method: "password",
        password: "form-test-credential",
      });
      return Response.json({ error: "duplicate host id" }, { status: 409 });
    }),
  );
  render(
    <QueryClientProvider client={client}>
      <Dialog.Root open>
        <RegisterHostForm
          close={vi.fn()}
          onPending={vi.fn()}
          onRegistered={vi.fn()}
        />
      </Dialog.Root>
    </QueryClientProvider>,
  );
  const user = userEvent.setup();
  await user.type(screen.getByLabelText("서버 IP 또는 호스트명"), "10.0.0.21");
  await user.type(screen.getByLabelText(/호스트 ID/), "target-01");
  await user.type(screen.getByLabelText("SSH 사용자"), "ubuntu");
  await user.click(screen.getByRole("radio", { name: "비밀번호" }));
  await user.type(
    screen.getByLabelText("SSH 비밀번호"),
    "form-test-credential",
  );
  const submit = screen.getByRole("button", {
    name: "호스트 등록",
  }) as HTMLButtonElement;
  expect(submit.disabled).toBe(true);
  await user.click(screen.getByRole("button", { name: "서버 지문 조회" }));
  const confirm = await screen.findByRole("checkbox");
  expect(submit.disabled).toBe(true);
  await user.click(confirm);
  expect(submit.disabled).toBe(false);
  await user.type(screen.getByLabelText("서버 IP 또는 호스트명"), "2");
  expect(submit.disabled).toBe(true);
  expect(screen.queryByRole("checkbox")).toBeNull();
  await user.click(screen.getByRole("button", { name: "서버 지문 조회" }));
  await user.click(await screen.findByRole("checkbox"));
  await user.click(submit);
  await screen.findByText("duplicate host id");
  await waitFor(() =>
    expect(
      (screen.getByLabelText("SSH 비밀번호") as HTMLInputElement).value,
    ).toBe(""),
  );
  expect(registrations).toBe(1);
  expect(JSON.stringify(client.getMutationCache().getAll())).not.toContain(
    "form-test-credential",
  );
  expect(JSON.stringify(client.getQueryCache().getAll())).not.toContain(
    "form-test-credential",
  );
  client.clear();
});

test("registers this machine as a host and checks it right away", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  const calls: string[] = [];
  let registered = false;
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string, options?: RequestInit) => {
      if (options?.method === "POST") calls.push(path);
      if (path.endsWith("/local-host")) {
        registered = true;
        return Response.json(
          { id: "local", kind: "local", status: "registered" },
          { status: 201 },
        );
      }
      if (path.endsWith("/hosts/local/check"))
        return Response.json({ id: "job", host_id: "local", events: [] });
      if (path.endsWith("/hosts"))
        return Response.json(
          registered
            ? [
                {
                  id: "local",
                  kind: "local",
                  address: "localhost",
                  port: 0,
                  status: "checking",
                  active_job: "job",
                },
              ]
            : [],
        );
      if (path.endsWith("/host")) return Response.json({ build_id: "x" });
      return Response.json([]);
    }),
  );
  render(
    <QueryClientProvider client={client}>
      <RemoteHostsPage />
    </QueryClientProvider>,
  );
  const user = userEvent.setup();
  await screen.findByText(/Farm을 실행할 호스트가 없습니다/);
  await user.click(
    screen.getAllByRole("button", { name: "로컬 호스트 등록" })[0],
  );
  await waitFor(() =>
    expect(calls).toEqual(["/api/v1/local-host", "/api/v1/hosts/local/check"]),
  );
  expect(await screen.findByText("LOCAL HOST")).toBeTruthy();
  expect(screen.queryByRole("button", { name: /인증 변경/ })).toBeNull();
  expect(screen.queryByRole("button", { name: "로컬 호스트 등록" })).toBeNull();
  client.clear();
});

test("can delete the local host's snapshots along with its farms", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  const deletes: string[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string, options?: RequestInit) => {
      if (options?.method === "DELETE") {
        deletes.push(path);
        return Response.json(
          { id: "job", host_id: "local", events: [] },
          { status: 202 },
        );
      }
      if (path.endsWith("/hosts"))
        return Response.json([
          {
            id: "local",
            kind: "local",
            address: "localhost",
            port: 0,
            status: "ready",
          },
        ]);
      if (path.endsWith("/snapshots"))
        return Response.json([{ id: "clean", status: "ready" }]);
      if (path.endsWith("/host")) return Response.json({ build_id: "x" });
      return Response.json([]);
    }),
  );
  render(
    <QueryClientProvider client={client}>
      <RemoteHostsPage />
    </QueryClientProvider>,
  );
  const user = userEvent.setup();
  await user.click(await screen.findByRole("button", { name: "등록 해제" }));
  const snapshots = await screen.findByRole("checkbox", {
    name: /스냅샷 1개도 삭제/,
  });
  expect(snapshots.getAttribute("aria-checked")).toBe("false");
  await user.click(snapshots);
  const dialog = screen.getByRole("dialog");
  const buttons = dialog.querySelectorAll("button");
  await user.click(
    [...buttons].find((b) => b.textContent === "등록 해제") as HTMLElement,
  );
  await waitFor(() =>
    expect(deletes).toEqual(["/api/v1/hosts/local?purge=true&snapshots=true"]),
  );
  client.clear();
});

test("shows a dial-in host's token once, in the command to run on it", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string, options?: RequestInit) => {
      expect(path).toBe("/api/v1/agent-hosts");
      expect(JSON.parse(String(options?.body))).toEqual({ id: "edge-01" });
      return Response.json(
        {
          host: { id: "edge-01", kind: "agent", connected: false },
          token: "farm_secret-token",
        },
        { status: 201 },
      );
    }),
  );
  const onRegistered = vi.fn().mockResolvedValue(undefined);
  render(
    <QueryClientProvider client={client}>
      <RegisterAgentHostDialog onRegistered={onRegistered} />
    </QueryClientProvider>,
  );
  const user = userEvent.setup();
  await user.click(
    screen.getByRole("button", { name: "에이전트 호스트 등록" }),
  );
  await user.type(screen.getByLabelText("호스트 ID"), "edge-01");
  await user.click(screen.getByRole("button", { name: "등록하고 토큰 받기" }));
  expect(
    await screen.findByText(
      "FARM_AGENT_TOKEN=farm_secret-token farm agent connect <coordinator URL>",
    ),
  ).toBeTruthy();
  expect(onRegistered).toHaveBeenCalledWith("edge-01");
  await user.click(screen.getByRole("button", { name: "완료" }));
  await user.click(
    screen.getByRole("button", { name: "에이전트 호스트 등록" }),
  );
  expect(screen.queryByText(/farm_secret-token/)).toBeNull();
  client.clear();
});
