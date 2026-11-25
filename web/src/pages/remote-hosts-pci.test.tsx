import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, test, vi } from "vitest";
import { RemoteHostsPage } from "./remote-hosts";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

const inspection = {
  os: "linux",
  arch: "x86_64",
  compatible: true,
  kvm_available: true,
  sudo_available: false,
  memory_total_mib: 37008,
  memory_available_mib: 30717,
  disk_available_mib: 259507,
  msb_version: "0.8.1",
  msb_compatible: true,
  docker_available: true,
  golden_image_available: true,
  vm_boot_verified: true,
  checked_at: 1794445792,
  pci_devices: [],
};

/** One vfio-pci device, one bindable Wi-Fi card, one device a stackbox holds, and three protected ones. */
const candidates = [
  {
    bdf: "0000:04:00.3",
    vendor_id: "1022",
    product_id: "149c",
    class: "0c0330",
    class_name: "usb",
    iommu_group: 12,
    driver: "vfio-pci",
    group_exclusive: true,
    vfio_ready: true,
    bindable: false,
  },
  {
    bdf: "0000:04:00.0",
    vendor_id: "10de",
    product_id: "2204",
    class: "030000",
    class_name: "gpu",
    iommu_group: 11,
    driver: "vfio-pci",
    group_exclusive: true,
    vfio_ready: true,
    bindable: false,
    owning_stackbox: "gpu-stack",
    protected_reason: "bound_to_stackbox",
  },
  {
    bdf: "0000:02:00.0",
    vendor_id: "8086",
    product_id: "2725",
    class: "028000",
    class_name: "network",
    iommu_group: 7,
    driver: "iwlwifi",
    group_exclusive: true,
    bindable: true,
  },
  {
    bdf: "0000:00:1d.0",
    vendor_id: "144d",
    product_id: "a80c",
    class: "010802",
    class_name: "nvme",
    iommu_group: 3,
    driver: "nvme",
    group_exclusive: true,
    bindable: false,
    protected_reason: "root_disk",
  },
  {
    bdf: "0000:03:00.0",
    vendor_id: "10ec",
    product_id: "8168",
    class: "020000",
    class_name: "network",
    iommu_group: 5,
    driver: "r8169",
    group_exclusive: true,
    bindable: false,
    protected_reason: "default_route_nic",
  },
  {
    bdf: "0000:00:1f.3",
    vendor_id: "8086",
    product_id: "a348",
    class: "040300",
    class_name: "audio",
    iommu_group: 8,
    driver: "snd_hda_intel",
    group_exclusive: false,
    bindable: false,
    protected_reason: "shared_iommu_group",
  },
];

/** `helperVersion` defaults to the current protocol (2) when the helper is available, else none. */
function hostWith(
  helper: boolean,
  extra: Record<string, unknown> = {},
  helperVersion: number | null = helper ? 2 : null,
) {
  return {
    id: "dev-01",
    kind: "ssh",
    address: "192.0.2.10",
    port: 22,
    username: "dev",
    status: "ready",
    auth_method: "private_key",
    fingerprint: "SHA256:test",
    agent: {
      artifact: {
        sha256: "a".repeat(64),
        os: "linux",
        arch: "x86_64",
        protocol: 1,
        supports_single_vm: true,
        build_id: "current",
      },
    },
    inspection: {
      ...inspection,
      pci_candidates: candidates,
      pci_bind_helper_available: helper,
      pci_bind_helper_version: helperVersion,
    },
    ...extra,
  };
}

const helperInstall = {
  script: "#!/bin/sh\n# stackbox-vfio-bind helper\necho install",
  helper_path: "/usr/local/sbin/stackbox-vfio-bind",
  sudoers_path: "/etc/sudoers.d/stackbox-vfio-bind",
};

/**
 * Answers the hosts API like the coordinator, and records every call it receives. A bind or
 * unbind creates a job; `/host-jobs` then reports it with `settle`'s status, the way the job runs
 * to its end.
 */
function renderPci(
  hosts: unknown[],
  settle: { status?: string; error?: string } = {},
) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  const calls: { url: string; method: string; body?: unknown }[] = [];
  const posted: Record<string, unknown>[] = [];
  const fetchMock = vi.fn(async (url: string, init?: RequestInit) => {
    const method = init?.method ?? "GET";
    calls.push({
      url,
      method,
      body: init?.body ? JSON.parse(String(init.body)) : undefined,
    });
    if (url.includes("/pci/bind") || url.includes("/pci/unbind")) {
      const job = {
        id: `job-${posted.length + 1}`,
        host_id: "dev-01",
        kind: url.includes("/pci/bind") ? "pci_bind" : "pci_unbind",
        status: "queued",
        created_at: 1,
        events: [],
        pci_bdf: JSON.parse(String(init?.body)).bdf,
      };
      posted.push({
        ...job,
        status: settle.status ?? "queued",
        error: settle.error,
      });
      return Response.json(job, { status: 202 });
    }
    if (url.endsWith("/host-jobs")) return Response.json(posted);
    if (url.includes("/pci/helper-install"))
      return Response.json(helperInstall);
    if (url.endsWith("/hosts")) return Response.json(hosts);
    if (url.endsWith("/host"))
      return Response.json({ build_id: "current" });
    return Response.json([]);
  });
  vi.stubGlobal("fetch", fetchMock);
  render(
    <QueryClientProvider client={client}>
      <RemoteHostsPage />
    </QueryClientProvider>,
  );
  return { calls };
}

/** The list row for one device, once the host list has loaded, so its buttons can be checked in place. */
async function rowOf(bdf: string) {
  const row = (await screen.findByText(bdf)).closest("li");
  if (!row) throw new Error(`no row for ${bdf}`);
  return within(row);
}

test("with the helper installed, binding a device asks first and then posts the bind", async () => {
  const { calls } = renderPci([hostWith(true)]);
  const user = userEvent.setup();

  expect(await screen.findByText("원클릭 바인딩 준비됨")).toBeTruthy();
  expect(screen.queryByRole("button", { name: "원클릭 헬퍼 설치" })).toBeNull();

  await user.click(
    (await rowOf("0000:02:00.0")).getByRole("button", { name: "vfio-pci 바인딩" }),
  );
  const dialog = await screen.findByRole("dialog");
  expect(within(dialog).getByText("0000:02:00.0 vfio-pci 바인딩")).toBeTruthy();
  expect(within(dialog).getByText("iwlwifi")).toBeTruthy();
  expect(calls.some((c) => c.url.includes("/pci/bind"))).toBe(false);

  await user.click(within(dialog).getByRole("button", { name: "바인딩 실행" }));
  await waitFor(() =>
    expect(
      calls.find((c) => c.url.includes("/pci/bind")),
    ).toMatchObject({
      url: "/api/v1/hosts/dev-01/pci/bind",
      method: "POST",
      body: { bdf: "0000:02:00.0" },
    }),
  );
  await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
});

test("with the helper installed, unbinding a vfio-pci device asks first and then posts the unbind", async () => {
  const { calls } = renderPci([hostWith(true)]);
  const user = userEvent.setup();

  await user.click((await rowOf("0000:04:00.3")).getByRole("button", { name: "바인딩 해제" }));
  const dialog = await screen.findByRole("dialog");
  expect(within(dialog).getByText("0000:04:00.3 바인딩 해제")).toBeTruthy();
  expect(calls.some((c) => c.url.includes("/pci/unbind"))).toBe(false);

  await user.click(within(dialog).getByRole("button", { name: "바인딩 해제 실행" }));
  await waitFor(() =>
    expect(
      calls.find((c) => c.url.includes("/pci/unbind")),
    ).toMatchObject({
      url: "/api/v1/hosts/dev-01/pci/unbind",
      method: "POST",
      body: { bdf: "0000:04:00.3" },
    }),
  );
});

test("a finished bind shows a green success banner that can be closed", async () => {
  renderPci([hostWith(true)], { status: "succeeded" });
  const user = userEvent.setup();

  await user.click(
    (await rowOf("0000:02:00.0")).getByRole("button", { name: "vfio-pci 바인딩" }),
  );
  await user.click(
    within(await screen.findByRole("dialog")).getByRole("button", { name: "바인딩 실행" }),
  );

  const text = "✓ 0000:02:00.0 vfio-pci 바인딩이 완료되었습니다.";
  const banner = (await screen.findByText(text)).closest(".remote-pci-banner");
  expect(banner?.className).toContain("tone-ok");
  expect(banner?.getAttribute("role")).toBe("status");

  await user.click(within(banner as HTMLElement).getByRole("button", { name: "닫기" }));
  expect(screen.queryByText(text)).toBeNull();
});

test("a finished unbind says the device's binding was released", async () => {
  renderPci([hostWith(true)], { status: "succeeded" });
  const user = userEvent.setup();

  await user.click((await rowOf("0000:04:00.3")).getByRole("button", { name: "바인딩 해제" }));
  await user.click(
    within(await screen.findByRole("dialog")).getByRole("button", { name: "바인딩 해제 실행" }),
  );

  const banner = (
    await screen.findByText("✓ 0000:04:00.3 바인딩이 해제되었습니다.")
  ).closest(".remote-pci-banner");
  expect(banner?.className).toContain("tone-ok");
});

test("a failed bind shows the failure and the helper's reason in a red alert", async () => {
  renderPci([hostWith(true)], {
    status: "failed",
    error: "0000:02:00.0 did not bind to vfio-pci",
  });
  const user = userEvent.setup();

  await user.click(
    (await rowOf("0000:02:00.0")).getByRole("button", { name: "vfio-pci 바인딩" }),
  );
  await user.click(
    within(await screen.findByRole("dialog")).getByRole("button", { name: "바인딩 실행" }),
  );

  const alert = await screen.findByRole("alert");
  expect(alert.textContent).toContain("0000:02:00.0 vfio-pci 바인딩이 실패했습니다.");
  expect(alert.textContent).toContain("0000:02:00.0 did not bind to vfio-pci");
  expect(screen.queryByText(/완료되었습니다/)).toBeNull();
});

test("after the helper install script is shown, its check shortcut runs the host check", async () => {
  const { calls } = renderPci([hostWith(false)]);
  const user = userEvent.setup();

  await user.click(await screen.findByRole("button", { name: "원클릭 헬퍼 설치" }));
  expect(await screen.findByText(helperInstall.sudoers_path)).toBeTruthy();
  expect(
    screen.getByText("설치 후 상단 'SSH 점검' 버튼을 누르면 원클릭 바인딩이 활성화됩니다."),
  ).toBeTruthy();

  await user.click(screen.getByRole("button", { name: "SSH 점검 실행" }));
  await waitFor(() =>
    expect(
      calls.some((c) => c.url === "/api/v1/hosts/dev-01/check" && c.method === "POST"),
    ).toBe(true),
  );
});

test("without the helper, the page offers the install script and per-device copy commands", async () => {
  renderPci([hostWith(false)]);
  const user = userEvent.setup();
  const writeText = vi.fn(async (_text: string) => {});
  Object.defineProperty(navigator, "clipboard", {
    value: { writeText },
    configurable: true,
  });

  await user.click(await screen.findByRole("button", { name: "원클릭 헬퍼 설치" }));
  expect(await screen.findByText(helperInstall.sudoers_path)).toBeTruthy();
  expect(screen.getByLabelText("헬퍼 설치 스크립트").textContent).toContain(
    "stackbox-vfio-bind helper",
  );

  // A device the helper would bind gets the manual bind command; a vfio-pci device, the unbind one.
  const wifi = await rowOf("0000:02:00.0");
  expect(wifi.queryByRole("button", { name: "바인딩 명령 복사" })).toBeTruthy();
  expect((await rowOf("0000:04:00.3")).queryByRole("button", { name: "바인딩 해제 명령 복사" })).toBeTruthy();
  expect(wifi.queryByRole("button", { name: "vfio-pci 바인딩" })).toBeNull();

  await user.click(wifi.getByRole("button", { name: "바인딩 명령 복사" }));
  const bindText = String(writeText.mock.calls[0]?.[0]);
  expect(bindText).toContain("echo 0000:02:00.0 | sudo tee /sys/bus/pci/drivers/vfio-pci/bind");
  expect(bindText).toContain("sudo chgrp kvm /dev/vfio/7");
  expect(await wifi.findByRole("button", { name: "복사됨" })).toBeTruthy();
});

test("a non-SSH host names its login user before the install script is fetched", async () => {
  const local = hostWith(false, { kind: "local", id: "local", username: undefined });
  const { calls } = renderPci([local]);
  const user = userEvent.setup();

  await user.click(await screen.findByRole("button", { name: "원클릭 헬퍼 설치" }));
  expect(calls.some((c) => c.url.includes("/pci/helper-install"))).toBe(false);
  await user.type(screen.getByLabelText("호스트 사용자명"), "ubuntu");
  await user.click(screen.getByRole("button", { name: "스크립트 불러오기" }));
  await waitFor(() =>
    expect(calls.some((c) => c.url === "/api/v1/hosts/local/pci/helper-install?user=ubuntu")).toBe(true),
  );
});

test("protected devices stay folded until opened, and show their reasons when they are", async () => {
  renderPci([hostWith(true)]);
  const user = userEvent.setup();

  const toggle = await screen.findByRole("button", {
    name: "시스템 보호·미분리 장치 3개 보기",
  });
  expect(screen.queryByText("0000:00:1d.0")).toBeNull();

  await user.click(toggle);
  expect(screen.getByText("0000:00:1d.0")).toBeTruthy();
  expect(screen.getByText("루트 디스크")).toBeTruthy();
  expect(screen.getByText("기본 라우트 NIC")).toBeTruthy();
  expect(screen.getByText("IOMMU 그룹 미분리")).toBeTruthy();
  // A protected device offers no bind button.
  expect((await rowOf("0000:00:1d.0")).queryByRole("button")).toBeNull();
});

test("a device a stackbox holds shows its owner and cannot be unbound", async () => {
  renderPci([hostWith(true)]);

  const row = await rowOf("0000:04:00.0");
  expect((row.getByRole("button", { name: "바인딩 해제" }) as HTMLButtonElement).disabled).toBe(true);
  expect(row.getByText("사용 중(gpu-stack)")).toBeTruthy();
  expect(row.getByText("Stackbox 점유 중")).toBeTruthy();
});

test("with the latest helper, the header offers a reinstall that opens the install script", async () => {
  const { calls } = renderPci([hostWith(true)]);
  const user = userEvent.setup();

  expect(await screen.findByText("원클릭 바인딩 준비됨")).toBeTruthy();
  const reinstall = screen.getByRole("button", { name: "헬퍼 재설치" });
  expect(reinstall.getAttribute("aria-expanded")).toBe("false");
  expect(screen.queryByRole("button", { name: "헬퍼 업데이트" })).toBeNull();
  expect(calls.some((c) => c.url.includes("/pci/helper-install"))).toBe(false);

  await user.click(reinstall);
  expect(await screen.findByText(helperInstall.sudoers_path)).toBeTruthy();
  expect(calls.some((c) => c.url === "/api/v1/hosts/dev-01/pci/helper-install")).toBe(true);
  expect(screen.getByLabelText("헬퍼 설치 스크립트").textContent).toContain("stackbox-vfio-bind helper");
  expect(screen.getByRole("button", { name: "헬퍼 재설치" }).getAttribute("aria-expanded")).toBe("true");
  expect(screen.queryByText(/구버전 헬퍼/)).toBeNull();
});

test("with an outdated v1 helper, the page asks for an update and copy commands stay per device", async () => {
  renderPci([hostWith(false, {}, 1)]);
  const user = userEvent.setup();

  expect(await screen.findByText("헬퍼 업데이트 필요 (v1)")).toBeTruthy();
  expect(screen.queryByText("원클릭 바인딩 준비됨")).toBeNull();
  expect(screen.queryByRole("button", { name: "원클릭 헬퍼 설치" })).toBeNull();

  await user.click(screen.getByRole("button", { name: "헬퍼 업데이트" }));
  expect(
    await screen.findByText(
      "호스트에 구버전 헬퍼(v1)가 설치되어 있습니다. 아래 최신 스크립트를 호스트에서 실행해 덮어쓴 뒤 'SSH 점검'을 누르세요.",
    ),
  ).toBeTruthy();
  expect(await screen.findByText(helperInstall.sudoers_path)).toBeTruthy();

  const wifi = await rowOf("0000:02:00.0");
  expect(wifi.queryByRole("button", { name: "바인딩 명령 복사" })).toBeTruthy();
  expect((await rowOf("0000:04:00.3")).queryByRole("button", { name: "바인딩 해제 명령 복사" })).toBeTruthy();
  expect(wifi.queryByRole("button", { name: "vfio-pci 바인딩" })).toBeNull();
});

test("a failed bind shows the helper's stderr reason verbatim", async () => {
  const reason = "stackbox-vfio-bind: 0000:02:00.0 is protected (root_disk) and cannot be passed through";
  renderPci([hostWith(true)], { status: "failed", error: reason });
  const user = userEvent.setup();

  await user.click(
    (await rowOf("0000:02:00.0")).getByRole("button", { name: "vfio-pci 바인딩" }),
  );
  await user.click(
    within(await screen.findByRole("dialog")).getByRole("button", { name: "바인딩 실행" }),
  );

  const alert = await screen.findByRole("alert");
  expect(alert.textContent).toContain("0000:02:00.0 vfio-pci 바인딩이 실패했습니다.");
  expect(alert.textContent).toContain(reason);
});

test("a failed unbind names the release, not the old 원복 wording", async () => {
  renderPci([hostWith(true)], { status: "failed", error: "helper exited 1" });
  const user = userEvent.setup();

  await user.click((await rowOf("0000:04:00.3")).getByRole("button", { name: "바인딩 해제" }));
  await user.click(
    within(await screen.findByRole("dialog")).getByRole("button", { name: "바인딩 해제 실행" }),
  );

  const alert = await screen.findByRole("alert");
  expect(alert.textContent).toContain("0000:04:00.3 바인딩 해제가 실패했습니다.");
  expect(alert.textContent).not.toContain("원복");
});
