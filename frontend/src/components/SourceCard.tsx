import { useState } from "react";
import { formatDb, sendsForSource } from "../lib";
import type { RouteProfileSnapshot, SourceBrief } from "../types";
import { LoopToggle, VuMeter } from "./ui";

export default function SourceCard({
  source,
  route,
  meterLevel,
  meterHint = "全局捕获峰值",
  isOn,
  onToggle,
  onRemove,
  onSetGain,
  onSetMuted,
}: {
  source: SourceBrief;
  route: RouteProfileSnapshot;
  meterLevel: number;
  meterHint?: string;
  isOn: boolean;
  onToggle: (sourceId: string, on: boolean) => void;
  onRemove: (sourceId: string) => void;
  onSetGain: (sendId: string, gainDb: number) => void;
  onSetMuted: (sendId: string, muted: boolean) => void;
}) {
  const [optionsOpen, setOptionsOpen] = useState(false);
  const sends = sendsForSource(route, source.id);
  // 增益/静音作用于该音源的首条 send
  const primarySend = sends[0];

  return (
    <div className={`node-card ${isOn ? "" : "is-disabled"}`}>
      <div className="node-card-body">
        <div className="node-top-row">
          <div className="node-info-left">
            <div className="node-app-icon">
              <svg
                width="18"
                height="18"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                strokeWidth="2"
                strokeLinecap="round"
                strokeLinejoin="round"
              >
                <polygon points="11 5 6 9 2 9 2 15 6 15 11 19 11 5" />
                <path d="M15.54 8.46a5 5 0 0 1 0 7.07" />
              </svg>
            </div>
            <div className="node-title-group">
              <span className="node-title">{source.display_name}</span>
              <span className="node-subtext">
                {source.process_id != null
                  ? `进程 PID ${source.process_id}`
                  : source.kind === "device_loopback"
                    ? "设备回环"
                    : "音频来源"}
              </span>
            </div>
          </div>
          <LoopToggle
            checked={isOn}
            title="切换音源开关"
            onChange={(v) => onToggle(source.id, v)}
          />
        </div>

        <div className="node-content-padding">
        <div className="meter-hint">{meterHint}</div>
        <div className="node-channels-wrapper">
          <div className="node-channels-list">
            <VuMeter level={meterLevel} label="1 (L)" align="left" />
              <VuMeter level={meterLevel} label="2 (R)" align="left" />
            </div>
            <div
              className="socket socket-right"
              data-socket-id={source.id}
              data-node-type="source"
              data-node-id={source.id}
              title="拖拽立体声连线到输出通道"
            />
          </div>

          <div
            className={`node-options-toggle ${optionsOpen ? "open" : ""}`}
            onClick={() => setOptionsOpen((v) => !v)}
          >
            <svg
              width="12"
              height="12"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
            >
              <polyline points="9 18 15 12 9 6" />
            </svg>
            <span>高级选项 (Options)</span>
          </div>
          <div className={`node-options-content ${optionsOpen ? "show" : ""}`}>
            {primarySend ? (
              <>
                <div className="option-row">
                  <span className="option-label">增益 (Gain)</span>
                  <div className="option-control-group">
                    <input
                      type="range"
                      className="device-slider"
                      min={-24}
                      max={12}
                      step={0.5}
                      value={primarySend.gain_db}
                      onChange={(e) =>
                        onSetGain(primarySend.id, Number(e.target.value))
                      }
                    />
                    <span className="option-val">{formatDb(primarySend.gain_db)}</span>
                  </div>
                </div>
                <div className="option-row">
                  <span className="option-label">静音 (Mute)</span>
                  <input
                    type="checkbox"
                    checked={primarySend.muted}
                    onChange={(e) => onSetMuted(primarySend.id, e.target.checked)}
                  />
                </div>
              </>
            ) : (
              <span className="option-hint">尚未连线，无法设置增益/静音</span>
            )}
            <button className="btn-remove-card" onClick={() => onRemove(source.id)}>
              移除音源
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
