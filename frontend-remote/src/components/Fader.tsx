import { useCallback, useRef, useState } from "react";

import { dbToFaderPos, faderPosToDb } from "../lib/protocol";

/** 长按进入微调模式的时间（毫秒）。 */
const FINE_MODE_HOLD_MS = 500;
/** 双击复位判定间隔（毫秒）。 */
const DOUBLE_TAP_MS = 300;
/** 微调模式灵敏度系数。 */
const FINE_SENSITIVITY = 0.35;

interface FaderProps {
  label: string;
  gainDb: number;
  onGainChange: (gainDb: number) => void;
  disabled?: boolean;
}

/** 触控推子：垂直滑动，双击复位 0dB，长按进入微调。 */
export default function Fader({ label, gainDb, onGainChange, disabled }: FaderProps) {
  const [dragging, setDragging] = useState(false);
  const [pos, setPos] = useState(() => dbToFaderPos(gainDb));
  const [fineMode, setFineMode] = useState(false);
  const dragRef = useRef<{
    pointerId: number;
    startY: number;
    startPos: number;
    lastTap: number;
    fineTimer: number;
  }>({ pointerId: -1, startY: 0, startPos: 0, lastTap: 0, fineTimer: 0 });

  const computePos = useCallback((clientY: number, startY: number, startPos: number, fine: boolean) => {
    const delta = (clientY - startY) / 220; // 推子轨道参考高度 220px
    const scaled = fine ? delta * FINE_SENSITIVITY : delta;
    return Math.min(1, Math.max(0, startPos - scaled));
  }, []);

  const onPointerDown = (event: React.PointerEvent<HTMLDivElement>) => {
    if (disabled) return;
    event.currentTarget.setPointerCapture(event.pointerId);
    const now = performance.now();
    const state = dragRef.current;
    // 双击复位：500ms 内第二次按下 → 回到 0dB。
    if (now - state.lastTap < DOUBLE_TAP_MS) {
      onGainChange(0);
      setPos(dbToFaderPos(0));
      state.lastTap = 0;
      return;
    }
    state.lastTap = now;
    state.pointerId = event.pointerId;
    state.startY = event.clientY;
    state.startPos = pos;
    state.fineTimer = window.setTimeout(() => setFineMode(true), FINE_MODE_HOLD_MS);
    setDragging(true);
  };

  const onPointerMove = (event: React.PointerEvent<HTMLDivElement>) => {
    if (!dragging || event.pointerId !== dragRef.current.pointerId) return;
    const next = computePos(event.clientY, dragRef.current.startY, dragRef.current.startPos, fineMode);
    setPos(next);
    onGainChange(faderPosToDb(next));
  };

  const onPointerUp = (event: React.PointerEvent<HTMLDivElement>) => {
    if (event.pointerId !== dragRef.current.pointerId) return;
    window.clearTimeout(dragRef.current.fineTimer);
    setFineMode(false);
    setDragging(false);
    dragRef.current.pointerId = -1;
  };

  const reset = () => {
    onGainChange(0);
    setPos(dbToFaderPos(0));
  };

  const percent = pos * 100;
  const db = faderPosToDb(pos);

  return (
    <div className={`fader ${dragging ? "fader-active" : ""} ${fineMode ? "fader-fine" : ""}`}>
      <div className="fader-db">{db > 0 ? `+${db.toFixed(1)}` : db.toFixed(1)}</div>
      <div
        className="fader-track"
        role="slider"
        aria-label={label}
        aria-valuemin={-60}
        aria-valuemax={6}
        aria-valuenow={Math.round(db)}
        aria-valuetext={`${db.toFixed(1)} dB`}
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={onPointerUp}
        onPointerCancel={onPointerUp}
        onDoubleClick={reset}
      >
        <div className="fader-fill" style={{ height: `${percent}%` }} />
        <div className="fader-knob" style={{ bottom: `calc(${percent}% - 9px)` }} />
        <div className="fader-zero" />
      </div>
      <div className="fader-label">{label}</div>
    </div>
  );
}
