import { describe, expect, it } from "vitest";
import {
  deviceAvailabilityLabel,
  isDeviceSelectable,
  sourceKindForDevice,
} from "./deviceRouting";
import type { DeviceBrief } from "./types";

function device(overrides: Partial<DeviceBrief>): DeviceBrief {
  return {
    id: "endpoint",
    name: "Device",
    flow: "capture",
    category: "input_mic",
    compatibility: "capture_ready",
    status: "active",
    format_description: null,
    ...overrides,
  };
}

describe("设备路由映射", () => {
  it.each(["input_mic", "input_loopback", "input_virtual"] as const)(
    "capture 分类 %s 始终使用 DeviceCapture",
    (category) => {
      expect(sourceKindForDevice(device({ category }))).toBe("device_capture");
    },
  );

  it("render endpoint 使用 DeviceLoopback", () => {
    expect(
      sourceKindForDevice(
        device({ flow: "render", category: "output", compatibility: "render_ready" }),
      ),
    ).toBe("device_loopback");
  });

  it("禁用不兼容或不可用的设备", () => {
    expect(isDeviceSelectable(device({}))).toBe(true);
    expect(isDeviceSelectable(device({ status: "unavailable" }))).toBe(false);
    expect(isDeviceSelectable(device({ compatibility: "unsupported" }))).toBe(false);
    expect(deviceAvailabilityLabel(device({ compatibility: "unsupported" }))).toBe("不兼容");
  });
});
