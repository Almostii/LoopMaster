import type { DeviceBrief } from "./types";

export type DeviceSourceKind = "device_capture" | "device_loopback";

export function sourceKindForDevice(device: DeviceBrief): DeviceSourceKind {
  return device.flow === "capture" ? "device_capture" : "device_loopback";
}

export function isDeviceSelectable(device: DeviceBrief): boolean {
  const expectedCompatibility =
    device.flow === "capture" ? "capture_ready" : "render_ready";
  return device.status === "active" && device.compatibility === expectedCompatibility;
}

export function deviceAvailabilityLabel(device: DeviceBrief): string {
  if (device.status !== "active") return device.status;
  if (device.compatibility === "unsupported") return "不兼容";
  return "可用";
}
