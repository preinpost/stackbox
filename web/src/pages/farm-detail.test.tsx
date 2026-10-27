import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, expect, test } from "vitest";
import type { Farm } from "../api/client";
import { NodeTopology } from "./farm-detail";

afterEach(cleanup);
const farm: Farm = {
  id: "workspace",
  status: "created",
  created_at: 0,
  updated_at: 0,
  plan: {
    components: ["keystone", "glance", "horizon"],
    memory_mib: 3584,
    disk_mib: 86016,
    warnings: [],
    nodes: [{ role: "all-in-one", cpus: 2, disk_mib: 20480, memory_mib: 3584 }],
  },
  nodes: [
    {
      name: "workspace-all-in-one",
      role: "all-in-one",
      memory_mib: 3584,
      status: "running",
    },
  ],
};

test("shows selected services inside one VM without overlay or separate role nodes", () => {
  const { container } = render(<NodeTopology farm={farm} />);
  expect(container.querySelectorAll(".topology-node")).toHaveLength(1);
  expect(screen.getByText("farm0 10.254.0.2 · 단일 VM")).toBeTruthy();
  // Services bind the fixed service address, not the per-start eth0 address.
  expect(screen.getAllByText("10.254.0.2").length).toBeGreaterThan(0);
  expect(screen.getByText("workspace-all-in-one")).toBeTruthy();
  expect(screen.getByText("Horizon")).toBeTruthy();
  expect(screen.queryByText("Deployer")).toBeNull();
  expect(container.textContent).not.toContain("vx0");
});

test("retains observed legacy and unclassified nodes instead of inventing a new layout", () => {
  const { container } = render(
    <NodeTopology
      farm={{
        ...farm,
        plan: null,
        nodes: [
          {
            name: "old-infra",
            role: "infra",
            memory_mib: 2560,
            status: "running",
          },
          { name: "old-other", role: null, memory_mib: 512, status: "stopped" },
        ],
      }}
    />,
  );
  expect(container.querySelectorAll(".topology-node")).toHaveLength(2);
  expect(screen.getByText("Infrastructure")).toBeTruthy();
  expect(screen.getByText("old-other")).toBeTruthy();
  expect(screen.queryByText("All-in-one")).toBeNull();
  expect(container.textContent).toContain("자동 변환하지 않습니다");
});
