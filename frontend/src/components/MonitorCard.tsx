import { useState } from "react";
import { formatDb, sendsForExternal } from "../lib";
import { DEVICE_STATUS_LABEL } from "../types";
import type { DeviceBrief, ExternalOutputBrief, RouteProfileSnapshot } from "../types";
import { LoopToggle, VuMeter } from "./ui";

function stopProp(e: React.MouseEvent) {
  e.stopPropagation();
}

export default function MonitorCard({
  external,
  device,
  route,
  meterL,
  meterR,
  isOn,
  isSelected,
  onToggle,
  onSetGain,
  onSetMuted,
  onRename,
  onSelect,
}: {
  external: ExternalOutputBrief;
  device: DeviceBrief | undefined;
  route: RouteProfileSnapshot;
  meterL: number;
  meterR: number;
  isOn: boolean;
  isSelected: boolean;
  onToggle: (externalId: string, on: boolean) => void;
  onSetGain: (sendId: string, gainDb: number) => void;
  onSetMuted: (sendId: string, muted: boolean) => void;
  onRename: (externalId: string, name: string) => void;
  onSelect: () => void;
}) {
  const [optionsOpen, setOptionsOpen] = useState(false);
  const [editingName, setEditingName] = useState(false);
  const [nameDraft, setNameDraft] = useState(external.display_name);
  const statusLabel = device ? DEVICE_STATUS_LABEL[device.status] : "未知";
  const sends = sendsForExternal(route, external.id);

  function commitName() {
    const next = nameDraft.trim();
    setEditingName(false);
    if (next && next !== external.display_name) {
      onRename(external.id, next);
    } else {
      setNameDraft(external.display_name);
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
                <line x1="15.54" y1="8.46" x2="19" y2="4" />
                <line x1="17.5" y1="9" x2="21" y2="7" />
                <line x1="13.5" y1="11" x2="21" y2="13" />
              </svg>
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
                      setNameDraft(external.display_name);
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
                    setNameDraft(external.display_name);
                    setEditingName(true);
                  }}
                >
                  {external.display_name}
                </span>
              )}
              <span className="node-subtext">状态：{statusLabel}</span>
            </div>
          </div>
          <div onClick={stopProp}>
            <LoopToggle
              checked={isOn}
              title="切换监听开关"
              onChange={(v) => onToggle(external.id, v)}
            />
          </div>
        </div>

        <div className="node-content-padding">
          <div className="node-channels-wrapper">
            <div
              className="socket socket-left"
              data-socket-id={`${external.id}-in`}
              data-node-type="external"
              data-node-id={external.id}
              title="输入插孔"
              onClick={stopProp}
            />
            <div className="node-channels-list">
              <VuMeter level={meterL} label="Channel 1 (L)" align="right" />
              <VuMeter level={meterR} label="Channel 2 (R)" align="right" />
            </div>
          </div>

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
            {sends.length > 0 ? sends.map((send, index) => (
              <div className="send-options" key={send.id}>
                <div className="send-options-title">连线 {index + 1}</div>
                <div className="option-row">
                  <span className="option-label">增益 (Gain)</span>
                  <div className="option-control-group">
                    <input
                      type="range"
                      className="device-slider"
                      min={-24}
                      max={12}
                      step={0.5}
                      value={send.gain_db}
                      onChange={(e) => onSetGain(send.id, Number(e.target.value))}
                    />
                    <span className="option-val">{formatDb(send.gain_db)}</span>
                  </div>
                </div>
                <div className="option-row">
                  <span className="option-label">静音 (Mute)</span>
                  <input
                    type="checkbox"
                    checked={send.muted}
                    onChange={(e) => onSetMuted(send.id, e.target.checked)}
                  />
                </div>
              </div>
            )) : (
              <span className="option-hint">尚未连线，无法设置增益</span>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
