import { useEffect, useRef, useState } from "react";

/** Loopback 风格红/青胶囊 On-Off 开关 */
export function LoopToggle({
  checked,
  onChange,
  title,
}: {
  checked: boolean;
  onChange: (value: boolean) => void;
  title?: string;
}) {
  return (
    <label className="loopback-toggle" title={title}>
      <input
        type="checkbox"
        checked={checked}
        onChange={(e) => onChange(e.target.checked)}
      />
      <span className="toggle-pill">
        <span className="toggle-text toggle-text-on">On</span>
        <span className="toggle-text toggle-text-off">Off</span>
        <span className="toggle-knob" />
      </span>
    </label>
  );
}

/** 双行电平表中的单个电平条（峰值驱动 + 衰减动画） */
export function VuMeter({
  level,
  label,
  align = "left",
  labelClass,
}: {
  level: number; // 0..100
  label?: string;
  align?: "left" | "right";
  /** 自定义标签样式类（如输出通道的宽标签 "Channel 1 (L)"） */
  labelClass?: string;
}) {
  const [shown, setShown] = useState(0);
  const rafRef = useRef<number | null>(null);

  useEffect(() => {
    if (rafRef.current !== null) cancelAnimationFrame(rafRef.current);
    const target = Math.min(100, Math.max(0, level));
    rafRef.current = requestAnimationFrame(() => {
      // 使用阻尼衰减，让电平表有实时动画质感
      setShown((prev) => {
        const diff = target - prev;
        const next = Math.abs(diff) < 0.5 ? target : prev + diff * 0.35;
        return next;
      });
    });
    return () => {
      if (rafRef.current !== null) cancelAnimationFrame(rafRef.current);
    };
  }, [level]);

  const defaultLeft = "label-source";
  const defaultRight = "label-monitor";
  const leftCls = labelClass ?? defaultLeft;
  const rightCls = labelClass ?? defaultRight;

  return (
    <div className="channel-row">
      {label && align === "left" && (
        <span className={`channel-label ${leftCls}`}>{label}</span>
      )}
      <div className="vu-meter-wrap">
        <div className="vu-meter-bar" style={{ width: `${shown}%` }} />
      </div>
      {label && align === "right" && (
        <span className={`channel-label ${rightCls}`}>{label}</span>
      )}
    </div>
  );
}
