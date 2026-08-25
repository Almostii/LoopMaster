import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  applyRouteEdit,
  getEngineState,
  getRouteSnapshot,
  listAudioProcesses,
  listDevices,
  onDeviceLost,
  onDeviceRestored,
  onEngineStateChanged,
  onEngineStatsChanged,
  requestReconnect,
  startEngine,
  stopEngine,
  type RouteEditRequest,
} from "./api";
import { formatError, freshId } from "./lib";
import type {
  DeviceBrief,
  EngineStateBrief,
  EngineStatsEvent,
  ProcessBrief,
  RouteProfileSnapshot,
} from "./types";

export interface Notice {
  kind: "ok" | "error" | "info";
  text: string;
}

export function useLoopMaster() {
  const [captureDevices, setCaptureDevices] = useState<DeviceBrief[]>([]);
  const [renderDevices, setRenderDevices] = useState<DeviceBrief[]>([]);
  const [processes, setProcesses] = useState<ProcessBrief[]>([]);
  const [route, setRoute] = useState<RouteProfileSnapshot>({
    sources: [],
    output_channels: [],
    external_outputs: [],
    sends: [],
  });
  const [engineState, setEngineState] = useState<EngineStateBrief>({
    state: "stopped",
    running: false,
    failed: false,
    last_error: null,
  });
  const [stats, setStats] = useState<EngineStatsEvent | null>(null);
  const [notice, setNotice] = useState<Notice | null>(null);
  const [loading, setLoading] = useState(false);

  const busyRef = useRef(false);

  const showNotice = useCallback((text: string, kind: Notice["kind"] = "ok") => {
    setNotice({ text, kind });
  }, []);

  // ---------- 只读刷新 ----------

  const refreshDevices = useCallback(async () => {
    try {
      const devices = await listDevices();
      setCaptureDevices(devices.filter((d) => d.flow === "capture"));
      setRenderDevices(devices.filter((d) => d.flow === "render"));
    } catch (e) {
      showNotice(formatError(e), "error");
    }
  }, [showNotice]);

  const refreshProcesses = useCallback(async () => {
    try {
      setProcesses(await listAudioProcesses());
    } catch (e) {
      showNotice(formatError(e), "error");
    }
  }, [showNotice]);

  const refreshRoute = useCallback(async () => {
    try {
      setRoute(await getRouteSnapshot());
    } catch (e) {
      showNotice(formatError(e), "error");
    }
  }, [showNotice]);

  const refreshEngineState = useCallback(async () => {
    try {
      setEngineState(await getEngineState());
    } catch (e) {
      showNotice(formatError(e), "error");
    }
  }, [showNotice]);

  const refreshAll = useCallback(async () => {
    setLoading(true);
    try {
      await Promise.all([
        refreshDevices(),
        refreshProcesses(),
        refreshRoute(),
        refreshEngineState(),
      ]);
    } finally {
      setLoading(false);
    }
  }, [refreshDevices, refreshProcesses, refreshRoute, refreshEngineState]);

  // ---------- 通用命令包装 ----------

  const runEdit = useCallback(
    async (req: RouteEditRequest, okMessage: string) => {
      if (busyRef.current) return;
      busyRef.current = true;
      try {
        await applyRouteEdit(req);
        showNotice(okMessage);
      } catch (e) {
        showNotice(formatError(e), "error");
      } finally {
        busyRef.current = false;
        await refreshRoute();
        await refreshEngineState();
      }
    },
    [showNotice, refreshRoute, refreshEngineState],
  );

  // ---------- 引擎控制 ----------

  const doStartEngine = useCallback(async () => {
    try {
      await startEngine();
      showNotice("引擎已启动");
    } catch (e) {
      showNotice(formatError(e), "error");
    } finally {
      await refreshEngineState();
    }
  }, [showNotice, refreshEngineState]);

  const doStopEngine = useCallback(async () => {
    try {
      await stopEngine();
      showNotice("引擎已停止");
    } catch (e) {
      showNotice(formatError(e), "error");
    } finally {
      await refreshEngineState();
    }
  }, [showNotice, refreshEngineState]);

  const doReconnect = useCallback(async () => {
    try {
      await requestReconnect();
      showNotice("已触发重连");
    } catch (e) {
      showNotice(formatError(e), "error");
    } finally {
      await refreshEngineState();
    }
  }, [showNotice, refreshEngineState]);

  // ---------- 拓扑编辑 ----------

  const addSource = useCallback(
    async (process: ProcessBrief) => {
      await runEdit(
        {
          op: "add_source",
          id: freshId("src"),
          kind: "process_loopback",
          display_name: `${process.name}（PID ${process.pid}）`,
          endpoint_id: null,
          process_id: process.pid,
        },
        "已添加音源（拓扑变更需重启引擎生效）",
      );
    },
    [runEdit],
  );

  const addOutputChannel = useCallback(async () => {
    await runEdit(
      {
        op: "add_output_channel",
        id: freshId("ch"),
        display_name: `输出通道 ${route.output_channels.length + 1}`,
      },
      "已添加输出通道（拓扑变更需重启引擎生效）",
    );
  }, [runEdit, route.output_channels.length]);

  const addExternalOutput = useCallback(
    async (device: DeviceBrief) => {
      await runEdit(
        {
          op: "add_external_output",
          id: freshId("out"),
          endpoint_id: device.id,
          display_name: device.name,
        },
        "已添加外部输出（拓扑变更需重启引擎生效）",
      );
    },
    [runEdit],
  );

  const removeSource = useCallback(
    async (id: string) => {
      await runEdit({ op: "remove_source", id }, "已移除音源");
    },
    [runEdit],
  );

  const removeOutputChannel = useCallback(
    async (id: string) => {
      await runEdit({ op: "remove_output_channel", id }, "已移除输出通道");
    },
    [runEdit],
  );

  const removeExternalOutput = useCallback(
    async (id: string) => {
      await runEdit({ op: "remove_external_output", id }, "已移除外部输出");
    },
    [runEdit],
  );

  /** 在 source 与 output_channel 之间添加一条 send */
  const addSend = useCallback(
    async (sourceId: string, outputChannelId: string) => {
      await runEdit(
        {
          op: "add_send",
          id: freshId("send"),
          source_id: sourceId,
          output_channel_id: outputChannelId,
        },
        "已添加连线（拓扑变更需重启引擎生效）",
      );
    },
    [runEdit],
  );

  /** 在 output_channel 与 external_output 之间添加一条 send */
  const addSendToOutput = useCallback(
    async (outputChannelId: string, externalOutputId: string) => {
      await runEdit(
        {
          op: "add_send_to_output",
          id: freshId("send"),
          output_channel_id: outputChannelId,
          external_output_id: externalOutputId,
        },
        "已添加连线（拓扑变更需重启引擎生效）",
      );
    },
    [runEdit],
  );

  const removeSend = useCallback(
    async (sendId: string) => {
      await runEdit({ op: "remove_send", send_id: sendId }, "已删除连线");
    },
    [runEdit],
  );

  const setSendEnabled = useCallback(
    async (sendId: string, enabled: boolean) => {
      await runEdit(
        { op: "set_send_enabled", send_id: sendId, enabled },
        enabled ? "连线已开启" : "连线已关闭",
      );
    },
    [runEdit],
  );

  const setSendMuted = useCallback(
    async (sendId: string, muted: boolean) => {
      await runEdit(
        { op: "set_send_muted", send_id: sendId, muted },
        muted ? "已静音" : "已取消静音",
      );
    },
    [runEdit],
  );

  const setSendGain = useCallback(
    async (sendId: string, gainDb: number) => {
      await runEdit(
        { op: "set_send_gain", send_id: sendId, gain_db: gainDb },
        "已更新增益",
      );
    },
    [runEdit],
  );

  // ---------- 事件订阅 ----------

  useEffect(() => {
    void refreshAll();

    const unState = onEngineStateChanged((payload) => {
      setEngineState((prev) => ({ ...prev, state: payload.state, running: payload.running }));
      setNotice({ text: `引擎状态：${payload.running ? "运行中" : "已停止"}`, kind: "info" });
    });
    const unStats = onEngineStatsChanged((payload) => setStats(payload));
    const unLost = onDeviceLost((endpointId) =>
      setNotice({ text: `设备已丢失：${endpointId}`, kind: "error" }),
    );
    const unRestored = onDeviceRestored((endpointId) =>
      setNotice({ text: `设备已恢复：${endpointId}`, kind: "info" }),
    );

    return () => {
      void unState.then((fn) => fn());
      void unStats.then((fn) => fn());
      void unLost.then((fn) => fn());
      void unRestored.then((fn) => fn());
    };
  }, [refreshAll]);

  const meterLevel = useMemo(() => {
    if (!stats) return 0;
    // captured_peak 约为 0..1；做 0..100 映射并夹紧
    const raw = Math.min(1, Math.max(0, stats.captured_peak));
    return Math.round(raw * 100);
  }, [stats]);

  return {
    captureDevices,
    renderDevices,
    processes,
    route,
    engineState,
    stats,
    notice,
    loading,
    meterLevel,
    setNotice,
    refreshAll,
    refreshDevices,
    refreshProcesses,
    doStartEngine,
    doStopEngine,
    doReconnect,
    addSource,
    addOutputChannel,
    addExternalOutput,
    removeSource,
    removeOutputChannel,
    removeExternalOutput,
    addSend,
    addSendToOutput,
    removeSend,
    setSendEnabled,
    setSendMuted,
    setSendGain,
  };
}

export type LoopMaster = ReturnType<typeof useLoopMaster>;
