import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, test, vi } from "vitest";
import { RemoteHostsPage } from "./remote-hosts";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

const host = {
  id: "dev-01",
  kind: "ssh",
  address: "192.0.2.10",
  port: 22,
  status: "ready",
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
};
const inspection = {
  os: "linux",
  arch: "x86_64",
  compatible: true,
  kvm_available: true,
  sudo_available: false,
  memory_total_mib: 37008,
  memory_available_mib: 30717,
  disk_available_mib: 259507,
  msb_version: "0.7.13",
  msb_compatible: true,
  docker_available: true,
  golden_image_available: true,
  vm_boot_verified: true,
  checked_at: 1794445792,
};
const custom = {
  ...inspection,
  custom_kernel:
    "/home/user/.local/share/farm/libkrunfw/72f012b2c2c8c33c/libkrunfw.so.5.6.1",
  mtty_available: true,
  gpu_kernel: {
    kernel: "custom",
    kernel_id: "72f012b2c2c8c33c",
    vfio: true,
    vfio_mdev: true,
    mtty_registered: true,
    mdev_types: [{ name: "mtty-1", available_instances: 24 }],
    kernel_release: "6.12.113",
  },
};

function renderHosts(hosts: unknown[]) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  vi.stubGlobal(
    "fetch",
    vi.fn(async (path: string) => {
      if (path.endsWith("/hosts")) return Response.json(hosts);
      if (path.endsWith("/host")) return Response.json({ build_id: "current" });
      return Response.json([]);
    }),
  );
  render(
    <QueryClientProvider client={client}>
      <RemoteHostsPage />
    </QueryClientProvider>,
  );
  return client;
}

/** Preparation installs the runner, so the page offers one button, not two. */
test("a prepared host has one environment button and no separate runner install", async () => {
  renderHosts([{ ...host, inspection: custom }]);
  expect(await screen.findByRole("button", { name: /환경 준비/ })).toBeTruthy();
  expect(screen.queryByRole("button", { name: /Farm 실행기 설치/ })).toBeNull();
  expect(
    screen.queryByRole("button", { name: /Farm 실행기 재설치/ }),
  ).toBeNull();
});

/** The kernel and GPU checks come from the verification VM, and the details open up. */
test("a host that booted the farm kernel shows its kernel, VFIO and mtty count", async () => {
  renderHosts([{ ...host, inspection: custom }]);
  expect(await screen.findByText("Farm 게스트 커널")).toBeTruthy();
  expect(screen.getByText(/72f012b2c2c8c33c 확인됨/)).toBeTruthy();
  expect(screen.getByText("GPU 커널 기능 (VFIO · mdev)")).toBeTruthy();
  expect(screen.getByText("VFIO · mdev")).toBeTruthy();
  expect(screen.getByText("mtty 가상 GPU")).toBeTruthy();
  expect(screen.getByText("mtty-1 · 24개 가능")).toBeTruthy();

  const user = userEvent.setup();
  await user.click(screen.getByText("커널·GPU 상세"));
  expect(screen.getByText("6.12.113")).toBeTruthy();
  expect(screen.getByText("/dev/vfio/vfio 준비됨")).toBeTruthy();
  expect(screen.getByText("/sys/class/mdev_bus/mtty 등록됨")).toBeTruthy();
});

/** A host on the stock kernel says why, and the GPU checks fail with that evidence. */
test("a host on the stock kernel shows the reason and no mtty devices", async () => {
  const reason =
    "the installed kernel was built for msb 0.7.12, and preparing this host installs 0.7.13";
  renderHosts([
    {
      ...host,
      inspection: {
        ...inspection,
        custom_kernel_reason: reason,
        mtty_available: false,
        gpu_kernel: {
          kernel: "stock",
          reason,
          vfio: false,
          vfio_mdev: false,
          mtty_registered: false,
          mdev_types: [],
          kernel_release: "6.1.0",
        },
      },
    },
  ]);
  expect(await screen.findByText("Farm 게스트 커널")).toBeTruthy();
  expect(screen.getAllByText("stock 커널").length).toBeGreaterThan(0);
  expect(screen.getByText("mtty 없음")).toBeTruthy();
  expect(screen.getByText("미탑재")).toBeTruthy();
  // The reason stands next to the checklist, not only behind the detail toggle.
  expect(screen.getByText(/stock 커널로 부팅합니다/).textContent).toContain(
    reason,
  );

  const user = userEvent.setup();
  await user.click(screen.getByText("커널·GPU 상세"));
  expect(screen.getByText(reason)).toBeTruthy();
  expect(screen.getByText("6.1.0")).toBeTruthy();
});

/** State written before the probe existed has no GPU kernel; the mtty flag it stored still
 * stands, and the checks ask for another preparation to fill in the details. */
test("a host prepared before the GPU probe keeps its mtty result and asks for the details", async () => {
  renderHosts([
    {
      ...host,
      inspection: {
        ...inspection,
        custom_kernel:
          "/home/user/.local/share/farm/libkrunfw/72f012b2c2c8c33c/libkrunfw.so.5.6.1",
        mtty_available: true,
      },
    },
  ]);
  expect(await screen.findByText("Farm 게스트 커널")).toBeTruthy();
  expect(screen.getByText("확인됨 · 상세는 환경 준비 후")).toBeTruthy();
  expect(screen.getAllByText("환경 준비 후 확인").length).toBe(1);
});
