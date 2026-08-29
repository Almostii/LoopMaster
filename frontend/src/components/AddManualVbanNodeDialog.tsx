import { useState } from "react";

/**
 * 手动添加 VBAN 网络节点对话框（mDNS 不可用时的回退路径）。
 * 用户输入目标电脑的显示名、IP、端口、流名等，确认后回调 `onSubmit`。
 */
export default function AddManualVbanNodeDialog({
  open,
  onClose,
  onSubmit,
}: {
  open: boolean;
  onClose: () => void;
  onSubmit: (params: {
    name: string;
    address: string;
    port: number;
    stream_name: string;
    sample_rate?: number;
    channels?: number;
  }) => void;
}) {
  const [name, setName] = useState("");
  const [address, setAddress] = useState("");
  const [port, setPort] = useState("6980");
  const [streamName, setStreamName] = useState("");
  const [sampleRate, setSampleRate] = useState("48000");
  const [channels, setChannels] = useState("2");

  if (!open) return null;

  function handleSubmit() {
    const nameTrim = name.trim() || address.trim() || "网络节点";
    const portNum = parseInt(port, 10);
    const rateNum = parseInt(sampleRate, 10);
    const chNum = parseInt(channels, 10);
    onSubmit({
      name: nameTrim,
      address: address.trim(),
      port: Number.isFinite(portNum) && portNum > 0 ? portNum : 6980,
      stream_name: streamName.trim(),
      sample_rate: Number.isFinite(rateNum) && rateNum > 0 ? rateNum : 48000,
      channels: Number.isFinite(chNum) && chNum > 0 ? chNum : 2,
    });
    // 提交后清空表单（下次打开重新填写）。
    setName("");
    setAddress("");
    setPort("6980");
    setStreamName("");
    setSampleRate("48000");
    setChannels("2");
    onClose();
  }

  return (
    <div className="modal-overlay" onClick={onClose}>
      <div className="modal-dialog" onClick={(e) => e.stopPropagation()}>
        <div className="modal-title">手动添加网络节点</div>
        <div className="modal-desc">在 mDNS 不可用或自动发现失败时，手动输入目标电脑的连接信息。</div>

        <div className="modal-form">
          <label className="modal-field">
            <span className="modal-label">显示名（可选）</span>
            <input
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="如：Studio-推流机"
              autoFocus
            />
          </label>
          <label className="modal-field">
            <span className="modal-label">IP 地址 *</span>
            <input
              value={address}
              onChange={(e) => setAddress(e.target.value)}
              placeholder="如：192.168.1.50"
            />
          </label>
          <div className="modal-row">
            <label className="modal-field">
              <span className="modal-label">VBAN 端口</span>
              <input
                value={port}
                onChange={(e) => setPort(e.target.value)}
                placeholder="6980"
              />
            </label>
            <label className="modal-field">
              <span className="modal-label">采样率 (Hz)</span>
              <input
                value={sampleRate}
                onChange={(e) => setSampleRate(e.target.value)}
                placeholder="48000"
              />
            </label>
            <label className="modal-field">
              <span className="modal-label">声道数</span>
              <input
                value={channels}
                onChange={(e) => setChannels(e.target.value)}
                placeholder="2"
              />
            </label>
          </div>
          <label className="modal-field">
            <span className="modal-label">流名（可选，默认取显示名）</span>
            <input
              value={streamName}
              onChange={(e) => setStreamName(e.target.value)}
              placeholder="留空则使用显示名"
            />
          </label>
        </div>

        <div className="modal-actions">
          <button type="button" className="btn-secondary" onClick={onClose}>
            取消
          </button>
          <button
            type="button"
            className="btn-primary"
            disabled={!address.trim()}
            onClick={handleSubmit}
          >
            添加
          </button>
        </div>
      </div>
    </div>
  );
}
