import { DEVICE_STATUS_LABEL } from "../types";
import type { DeviceBrief, ExternalOutputBrief } from "../types";
import { LoopToggle, VuMeter } from "./ui";

export default function MonitorCard({
  external,
  device,
  meterLevel,
  meterHint = "全局捕获峰值",
  isOn,
  onToggle,
  onRemove,
}: {
  external: ExternalOutputBrief;
  device: DeviceBrief | undefined;
  meterLevel: number;
  meterHint?: string;
  isOn: boolean;
  onToggle: (externalId: string, on: boolean) => void;
  onRemove: (externalId: string) => void;
}) {
  const statusLabel = device ? DEVICE_STATUS_LABEL[device.status] : "未知";

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
                <line x1="15.54" y1="8.46" x2="19" y2="4" />
                <line x1="17.5" y1="9" x2="21" y2="7" />
                <line x1="13.5" y1="11" x2="21" y2="13" />
              </svg>
            </div>
            <div className="node-title-group">
              <span className="node-title">{external.display_name}</span>
              <span className="node-subtext">状态：{statusLabel}</span>
            </div>
          </div>
          <LoopToggle
            checked={isOn}
            title="切换监听开关"
            onChange={(v) => onToggle(external.id, v)}
          />
        </div>

        <div className="node-content-padding">
          <div className="meter-hint">{meterHint}</div>
          <div className="node-channels-wrapper">
            <div
              className="socket socket-left"
              data-socket-id={`${external.id}-in`}
              data-node-type="external"
              data-node-id={external.id}
              title="输入插孔"
            />
            <div className="node-channels-list">
              <VuMeter level={meterLevel} label="Channel 1 (L)" align="right" />
              <VuMeter level={meterLevel} label="Channel 2 (R)" align="right" />
            </div>
          </div>

          <button
            className="btn-remove-card"
            onClick={() => onRemove(external.id)}
          >
            移除外部输出
          </button>
        </div>
      </div>
    </div>
  );
}
