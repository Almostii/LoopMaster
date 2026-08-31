import { useEffect, useRef } from "react";

interface MeterProps {
  /** [peak_db, rms_db]，由二进制 meter 帧驱动。 */
  value: [number, number] | undefined;
  orientation?: "horizontal" | "vertical";
}

/** 显示范围（dBFS）。 */
const METER_MIN_DB = -60;
const METER_MAX_DB = 6;

function dbToPercent(db: number): number {
  const clamped = Math.min(METER_MAX_DB, Math.max(METER_MIN_DB, db));
  return ((clamped - METER_MIN_DB) / (METER_MAX_DB - METER_MIN_DB)) * 100;
}

/**
 * 电平表：rAF 驱动 attack/decay 平滑与 peak-hold（与桌面 VuMeter 同策略）。
 * 输入值变化只写入 ref，渲染由动画帧驱动，避免高频 setState。
 */
export default function Meter({ value, orientation = "vertical" }: MeterProps) {
  const currentRef = useRef(0);
  const peakRef = useRef(0);
  const peakTimeRef = useRef(0);
  const barRef = useRef<HTMLDivElement>(null);
  const peakRefEl = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const valueRef = { current: value };
    let raf = 0;
    const step = (now: number) => {
      const target = valueRef.current ? dbToPercent(valueRef.current[1]) : 0;
      const state = { current: currentRef.current, peak: peakRef.current, peakTime: peakTimeRef.current };
      const attack = 0.5;
      const decay = 0.08;
      let next = state.current;
      if (target > state.current) {
        next = state.current + (target - state.current) * attack;
      } else {
        next = state.current + (target - state.current) * decay;
      }
      if (Math.abs(target - next) < 0.2) next = target;
      currentRef.current = Math.max(0, Math.min(100, next));

      if (target > state.peak) {
        peakRef.current = target;
        peakTimeRef.current = now;
      } else if (now - state.peakTime > 500) {
        peakRef.current = Math.max(state.current, peakRef.current + (state.current - peakRef.current) * 0.12);
      }

      if (barRef.current) barRef.current.style.height = `${currentRef.current}%`;
      if (peakRefEl.current) peakRefEl.current.style.height = `${peakRef.current}%`;
      raf = requestAnimationFrame(step);
    };
    raf = requestAnimationFrame(step);
    return () => cancelAnimationFrame(raf);
  }, [value]);

  return (
    <div className={`meter meter-${orientation}`}>
      <div className="meter-track">
        <div ref={barRef} className="meter-bar" />
        <div ref={peakRefEl} className="meter-peak" />
      </div>
    </div>
  );
}
