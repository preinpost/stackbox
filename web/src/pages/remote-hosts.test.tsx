import { Dialog } from "@base-ui/react/dialog";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, test, vi } from "vitest";
import { RegisterHostForm } from "./remote-hosts";

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
