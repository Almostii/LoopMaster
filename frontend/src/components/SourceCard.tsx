import { useState } from "react";
import { formatDb, sendsForSource } from "../lib";
import type { DeviceBrief, RouteProfileSnapshot, SourceBrief } from "../types";
import { LoopToggle, VuMeter } from "./ui";

function stopProp(e: React.MouseEvent) {
  e.stopPropagation();
}

export default function SourceCard({
  source,
  route,
  meterL,
  meterR,
  icon,
  isOn,
  isSelected,
  silentHint,
  onSwitchToDevice,
  onToggle,
  onSetGain,
  onSetMuted,
  onRename,
  onSelect,
}: {
  source: SourceBrief;
  route: RouteProfileSnapshot;
  meterL: number;
  meterR: number;
  icon?: string | null;
  isOn: boolean;
  isSelected: boolean;
  /** 非 null 时表示该进程回环无音频，给出建议改用同名设备捕获的候选列表 */
  silentHint?: DeviceBrief[] | null;
  onSwitchToDevice?: (device: DeviceBrief) => void;
  onToggle: (sourceId: string, on: boolean) => void;
  onSetGain: (sendId: string, gainDb: number) => void;
  onSetMuted: (sendId: string, muted: boolean) => void;
  onRename: (sourceId: string, name: string) => void;
  onSelect: () => void;
}) {
  const [optionsOpen, setOptionsOpen] = useState(false);
  const [editingName, setEditingName] = useState(false);
  const [nameDraft, setNameDraft] = useState(source.display_name);
  const sends = sendsForSource(route, source.id);
  // 增益/静音作用于该音源的首条 send
  const primarySend = sends[0];

  function commitName() {
    const next = nameDraft.trim();
    setEditingName(false);
    if (next && next !== source.display_name) {
      onRename(source.id, next);
    } else {
      setNameDraft(source.display_name);
    }
  }

  return (
    <div
      className={`node-card ${isOn ? "" : "is-disabled"} ${isSelected ? "is-selected" : ""}`}
      onClick={(e) => {
        e.stopPropagation();
        onSelect();
      }}
    >
      <div className="node-card-body">
        <div className="node-top-row">
          <div className="node-info-left">
            <div className="node-app-icon">
              {icon ? (
                <img src={icon} alt="" />
              ) : (
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
              )}
            </div>
            <div className="node-title-group">
              {editingName ? (
                <input
                  className="node-title-input"
                  autoFocus
                  value={nameDraft}
                  onChange={(e) => setNameDraft(e.target.value)}
                  onBlur={commitName}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") commitName();
                    if (e.key === "Escape") {
                      setNameDraft(source.display_name);
                      setEditingName(false);
                    }
                  }}
                  onClick={stopProp}
                />
              ) : (
                <span
                  className="node-title"
                  title="双击重命名"
                  onDoubleClick={(e) => {
                    e.stopPropagation();
                    setNameDraft(source.display_name);
                    setEditingName(true);
                  }}
                >
                  {source.display_name}
                </span>
              )}
            </div>
          </div>
          <div onClick={stopProp}>
            <LoopToggle
              checked={isOn}
              title="切换音源开关"
              onChange={(v) => onToggle(source.id, v)}
            />
          </div>
        </div>

        <div className="node-content-padding">
        <div className="node-channels-wrapper">
          <div className="node-channels-list">
            <VuMeter level={meterL} label="1 (L)" align="left" />
              <VuMeter level={meterR} label="2 (R)" align="left" />
            </div>
            <div
              className="socket socket-right"
              data-socket-id={source.id}
              data-node-type="source"
              data-node-id={source.id}
              title="拖拽立体声连线到输出通道"
              onClick={stopProp}
            />
          </div>

          {silentHint && silentHint.length > 0 && (
            <div className="node-switch-hint" onClick={stopProp}>
              <div className="switch-hint-title">
                该进程可能更适合用同名设备捕获：
              </div>
              <div className="switch-hint-devices">
                {silentHint.map((d) => (
                  <button
                    key={d.id}
                    className="switch-hint-btn"
                    onClick={() => onSwitchToDevice?.(d)}
                    title={`改用设备捕获：${d.name}`}
                  >
                    {d.name}
                  </button>
                ))}
              </div>
            </div>
          )}

          <div
            className={`node-options-toggle ${optionsOpen ? "open" : ""}`}
            onClick={(e) => {
              e.stopPropagation();
              setOptionsOpen((v) => !v);
            }}
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
            <span>Options</span>
          </div>
          <div className={`node-options-content ${optionsOpen ? "show" : ""}`} onClick={stopProp}>
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
          </div>
        </div>
      </div>
    </div>
  );
}
