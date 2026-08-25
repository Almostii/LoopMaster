import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  applyRouteEdit,
  getEngineState,
  getRouteSnapshot,
  getSettings,
  listAudioProcesses,
  listDevices,
  loadConfig,
  onDeviceLost,
  onDeviceRestored,
  onEngineStateChanged,
  onEngineStatsChanged,
  requestReconnect,
  saveConfig,
  startEngine,
  stopEngine,
  updateSettings as apiUpdateSettings,
  type RouteEditRequest,
} from "./api";
import { formatError, freshId } from "./lib";
import type {
  AppSettings,
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
  const [settings, setSettings] = useState<AppSettings>({
    theme: "light",
    start_on_boot: false,
    launch_hidden: false,
  });

  const busyRef = useRef(false);
  const engineStateRef = useRef(engineState);
  engineStateRef.current = engineState;

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

  // 拓扑类操作（增删节点或连线）会改变引擎结构，需重启引擎才能生效。
  const TOPOLOGY_OPS = new Set<string>([
    "add_source",
    "add_output_channel",
    "add_external_output",
    "remove_source",
    "remove_output_channel",
    "remove_external_output",
    "add_send",
    "add_send_to_output",
    "remove_send",
  ]);

  // send 级热更新（enabled/muted/gain）：直接下发给运行中的引擎，不需重启，
  // 彼此独立。因此绕过 busyRef 串行锁，避免并发调用被静默丢弃（一个节点
  // 可能有多条 send，开关需全部生效）。
  const SEND_OPS = new Set<string>([
    "set_send_enabled",
    "set_send_muted",
    "set_send_gain",
  ]);

  const runEdit = useCallback(
    async (req: RouteEditRequest, okMessage: string) => {
      const isSend = SEND_OPS.has(req.op);
      if (!isSend) {
        if (busyRef.current) return;
        busyRef.current = true;
      }
      try {
        await applyRouteEdit(req);
        if (isSend) {
          // send 级热更新：草稿与运行中引擎已同步，无需重启。
          showNotice(okMessage);
        } else {
          const isTopology = TOPOLOGY_OPS.has(req.op);
          if (isTopology && engineStateRef.current.running) {
            // 拓扑变更需重启引擎才生效，明确告知并自动重启。
            showNotice(`${okMessage}；引擎已重启以应用拓扑`);
            await stopEngine();
            await startEngine();
          } else {
            showNotice(okMessage);
          }
        }
      } catch (e) {
        showNotice(formatError(e), "error");
      } finally {
        if (!isSend) busyRef.current = false;
        // 自动持久化：每次编辑后把最新草稿写入配置文件（失败静默，不阻断操作）。
        await saveConfig().catch(() => {});
        await refreshRoute();
        await refreshEngineState();
      }
    },
    [showNotice, refreshRoute, refreshEngineState, stopEngine, startEngine],
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


  /** 新增音源并自动连到第一个输出通道（若该音源尚无连线）。 */
  const addSourceWithAutoConnect = useCallback(
    async (
      req: { kind: "process_loopback" | "device_capture" | "device_loopback"; display_name: string; endpoint_id: string | null; process_id: number | null },
    ) => {
      const srcId = freshId("src");
      await runEdit(
        {
          op: "add_source",
          id: srcId,
          kind: req.kind,
          display_name: req.display_name,
          endpoint_id: req.endpoint_id,
          process_id: req.process_id,
        },
        "已添加音源（拓扑变更需重启引擎生效）",
      );
      // 新建音源后，若已存在输出通道且该音源尚无连线，则自动连到第一个输出通道
      const snap = await getRouteSnapshot();
      const hasSend = snap.sends.some((s) => s.source === srcId);
      if (!hasSend && snap.output_channels.length > 0) {
        await runEdit(
          {
            op: "add_send",
            id: freshId("send"),
            source_id: srcId,
            output_channel_id: snap.output_channels[0].id,
          },
          "已添加连线（拓扑变更需重启引擎生效）",
        );
      }
    },
    [runEdit],
  );

function cleanProcessName(name: string): string {
  // 去掉 .exe / .EXE 等可执行后缀
  return name.replace(/\.exe$/i, "");
}

  /** 从进程回环添加音源（Process Loopback）。 */
  const addSourceFromProcess = useCallback(
    async (process: ProcessBrief) => {
      await addSourceWithAutoConnect({
        kind: "process_loopback",
        display_name: cleanProcessName(process.name),
        endpoint_id: null,
        process_id: process.pid,
      });
    },
    [addSourceWithAutoConnect],
  );

  /** 从设备添加音源：麦克风（device_capture）或设备回环（device_loopback）。 */
  const addSourceFromDevice = useCallback(
    async (device: DeviceBrief, kind: "device_capture" | "device_loopback") => {
      await addSourceWithAutoConnect({
        kind,
        display_name: device.name,
        endpoint_id: device.id,
        process_id: null,
      });
    },
    [addSourceWithAutoConnect],
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

      // 自动把新外部输出与第一个输出通道连线（若存在且尚未连线）。
      // 该 send 与 add_external_output 同属一次拓扑变更，start_engine 会重建引擎生效。
      const latest = await getRouteSnapshot();
      const channel = latest.output_channels[0];
      const external = latest.external_outputs[latest.external_outputs.length - 1];
      if (channel && external) {
        const existing = latest.sends.find(
          (s) =>
            s.output_channel === channel.id && s.external_output === external.id,
        );
        if (!existing) {
          await addSendToOutput(channel.id, external.id);
        }
      }
    },
    [runEdit, addSendToOutput],
  );

  // ---------- 默认拓扑初始化 ----------

  // 防止初始化并发/重复执行
  const defaultInitRef = useRef(false);

  /**
   * 当路由完全为空（无任何 source / output_channel / external_output）时，
   * 自动建立默认拓扑：仅默认创建 1 个输出通道，方便用户在此基础上添加音源
   * （自动连线）与手动添加外部输出。外部输出需用户选择真实设备，故不自动创建。
   * 仅在首启且路由为空时执行一次，避免每次启动重复追加通道。
   */
  const ensureDefaultTopology = useCallback(async () => {
    if (defaultInitRef.current) return;
    defaultInitRef.current = true;
    try {
      // 以实时快照判断路由是否已非空，防止已有配置下重复创建通道。
      const snap = await getRouteSnapshot();
      if (
        snap.sources.length > 0 ||
        snap.output_channels.length > 0 ||
        snap.external_outputs.length > 0
      ) {
        return;
      }
      await addOutputChannel();
    } catch (e) {
      showNotice(formatError(e), "error");
    }
  }, [addOutputChannel, showNotice]);

  const removeSend = useCallback(
    async (sendId: string) => {
      await runEdit({ op: "remove_send", id: sendId }, "已删除连线");
    },
    [runEdit],
  );

  const setSendEnabled = useCallback(
    async (sendId: string, enabled: boolean) => {
      await runEdit(
        { op: "set_send_enabled", id: sendId, enabled },
        enabled ? "连线已开启" : "连线已关闭",
      );
    },
    [runEdit],
  );

  const setSendMuted = useCallback(
    async (sendId: string, muted: boolean) => {
      await runEdit(
        { op: "set_send_muted", id: sendId, muted },
        muted ? "已静音" : "已取消静音",
      );
    },
    [runEdit],
  );

  const setSendGain = useCallback(
    async (sendId: string, gainDb: number) => {
      await runEdit(
        { op: "set_send_gain", id: sendId, gain_db: gainDb },
        "已更新增益",
      );
    },
    [runEdit],
  );

  // ---------- 节点重命名 ----------

  /** 重命名音源（仅改显示名，不影响拓扑；引擎需重启才反映到运行中图）。 */
  const renameSource = useCallback(
    async (id: string, displayName: string) => {
      await runEdit(
        { op: "set_source_name", id, display_name: displayName },
        "已重命名音源",
      );
    },
    [runEdit],
  );

  /** 重命名输出通道。 */
  const renameOutputChannel = useCallback(
    async (id: string, displayName: string) => {
      await runEdit(
        { op: "set_output_channel_name", id, display_name: displayName },
        "已重命名输出通道",
      );
    },
    [runEdit],
  );

  /** 重命名外部输出（Monitor）。 */
  const renameExternalOutput = useCallback(
    async (id: string, displayName: string) => {
      await runEdit(
        { op: "set_external_output_name", id, display_name: displayName },
        "已重命名外部输出",
      );
    },
    [runEdit],
  );

  // ---------- send 通道映射（channel map）编辑 ----------

  /**
   * 设置某条 send 的通道映射（input -> output 声道映射）。
   * 该变更无对应的引擎热更新命令，需重启引擎才能生效；若引擎正在运行会
   * 在 runEdit 内自动重启以应用（channel map 属拓扑级参数）。
   */
  const setSendChannelMap = useCallback(
    async (sendId: string, channelMap: [number, number][]) => {
      await runEdit(
        {
          op: "set_send_channel_map",
          id: sendId,
          channel_map: channelMap,
        },
        "已更新通道映射（重启引擎生效）",
      );
    },
    [runEdit],
  );

  // ---------- 事件订阅 ----------

  useEffect(() => {
    void (async () => {
      // 1) 设备/进程/引擎状态先行刷新（loadConfig 不依赖它们）。
      await Promise.all([refreshDevices(), refreshProcesses(), refreshEngineState()]);
      // 2) 尝试加载上次保存的路由配置。
      let loaded = false;
      try {
        loaded = await loadConfig();
      } catch (e) {
        showNotice(formatError(e), "error");
      }
      if (loaded) {
        // 已加载配置：直接刷新快照，不建立默认拓扑。
        await refreshRoute();
      } else {
        // 无配置文件：刷新空快照后建立默认拓扑（仅首启/清空时）。
        await refreshRoute();
        await ensureDefaultTopology();
        // 默认拓扑建立后自动持久化一次，避免下次启动又走默认初始化。
        await saveConfig().catch(() => {});
      }
    })();

    const unState = onEngineStateChanged((payload) => {
      // 状态事件仅携带 state/running；failed/last_error 等完整失败详情
      // 需重新拉取引擎状态快照，确保失败界面正确刷新。
      void refreshEngineState();
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
  }, [refreshAll, refreshEngineState]);

  const meterLevel = useMemo(() => {
    if (!stats) return 0;
    // captured_peak 约为 0..1；做 0..100 映射并夹紧
    const raw = Math.min(1, Math.max(0, stats.captured_peak));
    return Math.round(raw * 100);
  }, [stats]);

  // 把幅度（0..1）映射为 0..100 的电平值并夹紧。
  const ampToLevel = useCallback((amp: number) => {
    const raw = Math.min(1, Math.max(0, amp));
    return Math.round(raw * 100);
  }, []);

  // 每条 send 的逐通道（L/R）峰值，键为 send id，值为 [L(0..100), R(0..100)]。
  const sendMeter = useMemo(() => {
    const map: Record<string, [number, number]> = {};
    if (stats?.send_peaks) {
      for (const [id, [l, r]] of Object.entries(stats.send_peaks)) {
        map[id] = [ampToLevel(l), ampToLevel(r)];
      }
    }
    return map;
  }, [stats, ampToLevel]);

  // 聚合某节点所有相关 send 的逐通道峰值（取 max），得到该节点的 L/R 电平。
  // source / external 的 send 由 route 关联；channel 作为 send 的任一端参与。
  const nodeMeter = useCallback(
    (sendIds: string[]): [number, number] => {
      let l = 0;
      let r = 0;
      for (const id of sendIds) {
        const m = sendMeter[id];
        if (m) {
          if (m[0] > l) l = m[0];
          if (m[1] > r) r = m[1];
        }
      }
      return [l, r];
    },
    [sendMeter],
  );

  const sourceSendIds = useCallback(
    (sourceId: string) => route.sends.filter((s) => s.source === sourceId).map((s) => s.id),
    [route.sends],
  );
  const externalSendIds = useCallback(
    (externalId: string) =>
      route.sends.filter((s) => s.external_output === externalId).map((s) => s.id),
    [route.sends],
  );
  const channelSendIds = useCallback(
    (channelId: string) =>
      route.sends.filter((s) => s.output_channel === channelId).map((s) => s.id),
    [route.sends],
  );

  /** 从后端加载应用设置并同步到本地 state。 */
  const loadSettings = useCallback(async () => {
    try {
      const s = await getSettings();
      setSettings(s);
    } catch (e) {
      console.error("加载设置失败:", e);
    }
  }, []);

  /** 更新应用设置并持久化，成功后同步本地 state。 */
  const updateSettings = useCallback(
    async (patch: Partial<AppSettings>) => {
      const next = await apiUpdateSettings({
        theme: patch.theme,
        start_on_boot: patch.start_on_boot,
        launch_hidden: patch.launch_hidden,
      });
      setSettings(next);
      return next;
    },
    [],
  );

  return {
    captureDevices,
    renderDevices,
    processes,
    route,
    engineState,
    stats,
    notice,
    loading,
    settings,
    loadSettings,
    updateSettings,
    meterLevel,
    sendMeter,
    nodeMeter,
    sourceSendIds,
    externalSendIds,
    channelSendIds,
    setNotice,
    refreshAll,
    refreshDevices,
    refreshProcesses,
    doStartEngine,
    doStopEngine,
    doReconnect,
    addSourceFromProcess,
    addSourceFromDevice,
    addOutputChannel,
    addExternalOutput,
    removeSource,
    removeOutputChannel,
    removeExternalOutput,
    addSend,
    addSendToOutput,
    ensureDefaultTopology,
    removeSend,
    setSendEnabled,
    setSendMuted,
    setSendGain,
    renameSource,
    renameOutputChannel,
    renameExternalOutput,
    setSendChannelMap,
  };
}

export type LoopMaster = ReturnType<typeof useLoopMaster>;
