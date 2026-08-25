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
    <div className={`channel-row ${align === "right" ? "channel-row-right" : ""}`}>
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

/**
 * 单条 send 的通道映射（channel map）编辑器。
 *
 * `channelMap` 为 `[input, output]` 声道对的列表（空列表表示 identity 映射）。
 * 提供“交换 L/R”快捷操作，以及手动编辑映射项。所有变更通过 `onChange` 上报。
 */
export function ChannelMapEditor({
  channelMap,
  onChange,
}: {
  channelMap: [number, number][];
  onChange: (next: [number, number][]) => void;
}) {
  // 默认展示 2 条映射（L->L, R->R），便于编辑；空映射也按此展开。
  const rows: [number, number][] =
    channelMap.length > 0
      ? channelMap
      : ([
          [0, 0],
          [1, 1],
        ] as [number, number][]);

  function update(index: number, which: "in" | "out", value: number) {
    const next = rows.map((r) => [...r] as [number, number]);
    next[index][which === "in" ? 0 : 1] = value;
    onChange(next);
  }

  function swap() {
    onChange(rows.map(([a, b]) => [b, a] as [number, number]));
  }

  return (
    <div className="channel-map-editor">
      <div className="channel-map-head">
        <span>通道映射 (In → Out)</span>
        <button
          type="button"
          className="btn-mini"
          title="交换左右声道"
          onClick={(e) => {
            e.stopPropagation();
            swap();
          }}
        >
          交换 L/R
        </button>
      </div>
      {rows.map(([a, b], i) => (
        <div className="channel-map-row" key={i}>
          <input
            type="number"
            min={0}
            max={15}
            value={a}
            onClick={(e) => e.stopPropagation()}
            onChange={(e) => update(i, "in", Number(e.target.value))}
          />
          <span className="channel-map-arrow">→</span>
          <input
            type="number"
            min={0}
            max={15}
            value={b}
            onClick={(e) => e.stopPropagation()}
            onChange={(e) => update(i, "out", Number(e.target.value))}
          />
        </div>
      ))}
      <span className="option-hint">修改后需重启引擎生效（仅更新草稿）。</span>
    </div>
  );
}
