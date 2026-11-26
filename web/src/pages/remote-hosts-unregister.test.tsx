import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, test, vi } from "vitest";
import type { HostJob } from "../api/client";
import { HostJobHistory, RemoteHostsPage } from "./remote-hosts";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

const removeScript = [
  "set -u",
  "rm -f '/etc/sudoers.d/stackbox-vfio' && echo 'stackbox: removed /etc/sudoers.d/stackbox-vfio'",
  "rm -f '/usr/local/libexec/stackbox-vfio-bind' && echo 'stackbox: removed /usr/local/libexec/stackbox-vfio-bind'",
].join("\n");

/** Stubs the page's API: the local host, its empty snapshot list, and the helper removal script. */
function stubApi(deletes: string[], gets: string[]) {
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
      gets.push(path);
      if (path.endsWith("/pci/helper-remove"))
        return Response.json({
          script: `sudo -v && sudo bash <<'STACKBOX_VFIO_EOF'\n${removeScript}\nSTACKBOX_VFIO_EOF\n`,
          helper_path: "/usr/local/libexec/stackbox-vfio-bind",
          sudoers_path: "/etc/sudoers.d/stackbox-vfio",
        });
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
      if (path.endsWith("/snapshots")) return Response.json([]);
      if (path.endsWith("/host")) return Response.json({ build_id: "x" });
      return Response.json([]);
    }),
  );
}

function renderPage() {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  render(
    <QueryClientProvider client={client}>
      <RemoteHostsPage />
    </QueryClientProvider>,
  );
  return client;
}

async function openRemoveDialog() {
  const user = userEvent.setup();
  await user.click(await screen.findByRole("button", { name: "등록 해제" }));
  const dialog = await screen.findByRole("dialog");
  return { user, dialog };
}

test("the remote-files box is off by default and only selectable while the VMs are removed", async () => {
  const deletes: string[] = [];
  stubApi(deletes, []);
  const client = renderPage();
  const { user } = await openRemoveDialog();
  const files = await screen.findByRole("checkbox", {
    name: /골든 이미지 그룹도 제거/,
  });
  expect(files.getAttribute("aria-checked")).toBe("false");
  expect(files.hasAttribute("data-disabled")).toBe(false);

  // Turning the VM removal off disables the box, and the script is not offered.
  const purge = screen.getByRole("checkbox", { name: /VM·디스크도 삭제/ });
  await user.click(purge);
  expect(
    screen
      .getByRole("checkbox", { name: /골든 이미지 그룹도 제거/ })
      .hasAttribute("data-disabled"),
  ).toBe(true);
  expect(screen.queryByLabelText("헬퍼 제거 스크립트")).toBeNull();
  client.clear();
});

test("with the VMs removed, the remote-files box sends remote_files and shows the helper removal script", async () => {
  const deletes: string[] = [];
  const gets: string[] = [];
  stubApi(deletes, gets);
  const client = renderPage();
  const { user, dialog } = await openRemoveDialog();
  await user.click(
    await screen.findByRole("checkbox", { name: /골든 이미지 그룹도 제거/ }),
  );
  // The root-side script is shown for the operator; it is fetched, never run here.
  const script = await screen.findByLabelText("헬퍼 제거 스크립트");
  expect(script.textContent).toContain("rm -f '/etc/sudoers.d/stackbox-vfio'");
  const sudoers = script.textContent?.indexOf("/etc/sudoers.d/stackbox-vfio") ?? -1;
  const helper = script.textContent?.indexOf("/usr/local/libexec/stackbox-vfio-bind") ?? -1;
  expect(sudoers).toBeGreaterThan(-1);
  expect(sudoers).toBeLessThan(helper);
  expect(gets.some((path) => path.endsWith("/pci/helper-remove"))).toBe(true);
  await user.click(
    [...dialog.querySelectorAll("button")].find(
      (b) => b.textContent === "등록 해제",
    ) as HTMLElement,
  );
  await waitFor(() =>
    expect(deletes).toEqual([
      "/api/v1/hosts/local?purge=true&remote_files=true",
    ]),
  );
  client.clear();
});

test("without the remote-files box the delete URL is unchanged", async () => {
  const deletes: string[] = [];
  stubApi(deletes, []);
  const client = renderPage();
  const { dialog } = await openRemoveDialog();
  await userEvent.setup().click(
    [...dialog.querySelectorAll("button")].find(
      (b) => b.textContent === "등록 해제",
    ) as HTMLElement,
  );
  await waitFor(() => expect(deletes).toEqual(["/api/v1/hosts/local?purge=true"]));
  client.clear();
});

test("the other stackboxes' VMs box is off by default and only selectable while the VMs are removed", async () => {
  const deletes: string[] = [];
  stubApi(deletes, []);
  const client = renderPage();
  const { user } = await openRemoveDialog();
  const others = screen.getByRole("checkbox", {
    name: /다른 stackbox가 이 서버에 만든 VM도 삭제/,
  });
  expect(others.getAttribute("aria-checked")).toBe("false");
  expect(others.hasAttribute("data-disabled")).toBe(false);
  // Turning the VM removal off disables it too.
  await user.click(screen.getByRole("checkbox", { name: /VM·디스크도 삭제/ }));
  expect(
    screen
      .getByRole("checkbox", { name: /다른 stackbox가 이 서버에 만든 VM도 삭제/ })
      .hasAttribute("data-disabled"),
  ).toBe(true);
  expect(screen.queryByText(/다른 coordinator의 stackbox가 이 서버에서/)).toBeNull();
  client.clear();
});

test("checking the other stackboxes' VMs box sends all_vms with the purge", async () => {
  const deletes: string[] = [];
  stubApi(deletes, []);
  const client = renderPage();
  const { user, dialog } = await openRemoveDialog();
  await user.click(
    screen.getByRole("checkbox", { name: /다른 stackbox가 이 서버에 만든 VM도 삭제/ }),
  );
  expect(
    screen
      .getByRole("checkbox", { name: /다른 stackbox가 이 서버에 만든 VM도 삭제/ })
      .getAttribute("aria-checked"),
  ).toBe("true");
  expect(dialog.textContent).toContain("다른 coordinator의 stackbox가 이 서버에서 쓰는 VM");
  await user.click(
    [...dialog.querySelectorAll("button")].find(
      (b) => b.textContent === "등록 해제",
    ) as HTMLElement,
  );
  await waitFor(() =>
    expect(deletes).toEqual(["/api/v1/hosts/local?purge=true&all_vms=true"]),
  );
  client.clear();
});

test("a purge that kept other coordinators' VMs names the shared files it kept and why", () => {
  const job = {
    id: "job-11",
    host_id: "dev-01",
    kind: "purge",
    status: "succeeded",
    created_at: 1_700_000_000,
    events: [
      { at: 1_700_000_001, message: "Removed 1 reserved VM(s)" },
      {
        at: 1_700_000_002,
        message: "Kept 1 VM reservation(s) of other stackbox coordinators on the host",
      },
      {
        at: 1_700_000_003,
        message:
          "Left on the host: golden snapshot groups, msb and ~/.local/share/stackbox (kept: 1 VM reservation(s) of other stackbox coordinators still use this host)",
      },
      { at: 1_700_000_004, message: "Host removed" },
    ],
    error: null,
  } as unknown as HostJob;
  const { container } = render(<HostJobHistory job={job} />);
  const notice = container.querySelector(".host-job-leftovers") as HTMLElement;
  expect(
    within(notice).getAllByRole("listitem").map((li) => li.textContent),
  ).toEqual([
    "golden snapshot groups, msb and ~/.local/share/stackbox (kept: 1 VM reservation(s) of other stackbox coordinators still use this host)",
  ]);
  // The directory is shared, so no by-hand command is offered for it.
  expect(notice.querySelector("code")).toBeNull();
});

test("a purge that left items behind opens its history and names each one with the command to remove it", () => {
  const job = {
    id: "job-9",
    host_id: "dev-01",
    kind: "purge",
    status: "succeeded",
    created_at: 1_700_000_000,
    events: [
      { at: 1_700_000_001, message: "Removed 2 reserved VM(s)" },
      {
        at: 1_700_000_002,
        message:
          "Left on the host: msb and libkrunfw (kept on purpose) | the host's files (failed) | the rest of the host's files (the cleanup timed out on the host) | the host's golden groups. To remove the directory by hand, run on the host: rm -rf ~/.local/share/stackbox",
      },
      { at: 1_700_000_003, message: "Host removed" },
    ],
    error: null,
  } as unknown as HostJob;
  const { container } = render(<HostJobHistory job={job} />);
  const details = container.querySelector("details") as HTMLDetailsElement;
  expect(details.open).toBe(true);
  // A purge is labelled as the removal it is, not as an environment preparation.
  expect(container.querySelector("summary .job-kind")?.textContent).toBe("등록 해제");
  const notice = container.querySelector(".host-job-leftovers") as HTMLElement;
  const items = within(notice).getAllByRole("listitem").map((li) => li.textContent);
  expect(items).toEqual([
    "msb and libkrunfw (kept on purpose)",
    "the host's files (failed)",
    "the rest of the host's files (the cleanup timed out on the host)",
    "the host's golden groups",
  ]);
  expect(notice.querySelector("code")?.textContent).toBe(
    "rm -rf ~/.local/share/stackbox",
  );
});

test("a purge that left nothing behind shows no leftover notice and stays folded", () => {
  const job = {
    id: "job-10",
    host_id: "dev-01",
    kind: "purge",
    status: "succeeded",
    created_at: 1_700_000_000,
    events: [{ at: 1_700_000_001, message: "Host removed" }],
    error: null,
  } as unknown as HostJob;
  const { container } = render(<HostJobHistory job={job} />);
  expect(container.querySelector(".host-job-leftovers")).toBeNull();
  expect((container.querySelector("details") as HTMLDetailsElement).open).toBe(false);
});
