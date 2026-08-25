import type { ChannelBrief } from "../types";
import { VuMeter } from "./ui";

function stopProp(e: React.MouseEvent) {
  e.stopPropagation();
}

export default function ChannelCard({
  channel,
  meterLevel,
  isSelected,
  onRemove,
  onSelect,
}: {
  channel: ChannelBrief;
  meterLevel: number;
  isSelected: boolean;
  onRemove: (channelId: string) => void;
  onSelect: () => void;
}) {
  return (
    <div
      className={`node-card ${isSelected ? "is-selected" : ""}`}
      onClick={(e) => {
        e.stopPropagation();
        onSelect();
      }}
    >
      <div className="node-card-body">
        <div className="node-top-row">
          <div className="node-title-group">
            <span className="node-title">{channel.display_name}</span>
          </div>
          <button
            className="btn-remove-icon"
            title="移除输出通道"
            onClick={(e) => {
              e.stopPropagation();
              onRemove(channel.id);
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
              <line x1="5" y1="12" x2="19" y2="12" />
            </svg>
          </button>
        </div>

        <div className="node-content-padding">
          <div className="node-channels-wrapper">
            <div
              className="socket socket-left"
              data-socket-id={`${channel.id}-in`}
              data-node-type="output"
              data-node-id={channel.id}
              title="输入插孔"
              onClick={stopProp}
            />
            <div className="node-channels-list">
              <VuMeter
                level={meterLevel}
                label="Channel 1 (L)"
                align="left"
                labelClass="label-channel"
              />
              <VuMeter
                level={meterLevel}
                label="Channel 2 (R)"
                align="left"
                labelClass="label-channel"
              />
            </div>
            <div
              className="socket socket-right"
              data-socket-id={`${channel.id}-out`}
              data-node-type="output"
              data-node-id={channel.id}
              title="输出插孔"
              onClick={stopProp}
            />
          </div>
        </div>
      </div>
    </div>
  );
}
