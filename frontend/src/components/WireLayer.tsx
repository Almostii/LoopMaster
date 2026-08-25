import { useEffect, useRef, useState } from "react";
import { createBezierPath } from "../lib";
import type { WireSpec } from "../lib";

interface Point {
  x: number;
  y: number;
}

function isRightSocket(el: Element): boolean {
  return el.classList.contains("socket-right");
}
function isLeftSocket(el: Element): boolean {
  return el.classList.contains("socket-left");
}

/** 从右插孔拖出、放到左插孔视为合法连接 */
function isValidPair(a: Element, b: Element): boolean {
  if (a === b) return false;
  const aCard = a.closest(".node-card");
  const bCard = b.closest(".node-card");
  if (aCard && bCard && aCard === bCard) return false;
  return (
    (isRightSocket(a) && isLeftSocket(b)) ||
    (isLeftSocket(a) && isRightSocket(b))
  );
}

export default function WireLayer({
  wires,
  svgRef,
  onConnect,
  onWireClick,
  selectedWireId,
}: {
  wires: WireSpec[];
  svgRef: React.RefObject<SVGSVGElement | null>;
  onConnect: (fromSocketId: string, toSocketId: string) => void;
  onWireClick: (wireId: string) => void;
  selectedWireId: string | null;
}) {
  const [temp, setTemp] = useState<{ from: Point; to: Point } | null>(null);
  const dragRef = useRef<{
    sourceEl: Element;
    sourceIsRight: boolean;
    hoverEl: Element | null;
  } | null>(null);
  const [, forceRender] = useState(0);

  // 每次路由变化后重算连线坐标
  useEffect(() => {
    forceRender((v) => v + 1);
  }, [wires]);

  function socketCenter(el: Element, svgRect: DOMRect): Point {
    const rect = el.getBoundingClientRect();
    return {
      x: rect.left + rect.width / 2 - svgRect.left,
      y: rect.top + rect.height / 2 - svgRect.top,
    };
  }

  // 事件：在 SVG 的父容器（拓扑视口）上做事件委托，捕获插孔拖拽与连线点击
  useEffect(() => {
    const container = svgRef.current?.parentElement;
    if (!container) return;

    function onPointerDown(e: PointerEvent) {
      const target = e.target as Element;
      if (target.classList.contains("socket")) {
        e.preventDefault();
        e.stopPropagation();
        const svg = svgRef.current!;
        const svgRect = svg.getBoundingClientRect();
        const p1 = socketCenter(target, svgRect);
        dragRef.current = {
          sourceEl: target,
          sourceIsRight: isRightSocket(target),
          hoverEl: null,
        };
        setTemp({
          from: p1,
          to: { x: e.clientX - svgRect.left, y: e.clientY - svgRect.top },
        });
      } else if (target.closest("path.wire")) {
        const path = target.closest("path.wire") as SVGPathElement;
        const wireId = path.dataset.wireId;
        if (wireId) onWireClick(wireId);
      }
    }

    container.addEventListener("pointerdown", onPointerDown);
    return () => container.removeEventListener("pointerdown", onPointerDown);
  }, [svgRef, onWireClick]);

  // 全局鼠标移动与松开：绘制临时连线 + 吸附
  useEffect(() => {
    function onMove(e: MouseEvent) {
      const drag = dragRef.current;
      if (!drag || !svgRef.current) return;
      const svgRect = svgRef.current.getBoundingClientRect();
      const p1 = socketCenter(drag.sourceEl, svgRect);

      // 通过 elementFromPoint 判断当前悬停的插孔
      let hover: Element | null = null;
      const hit = document.elementFromPoint(e.clientX, e.clientY);
      if (hit && hit.classList.contains("socket") && isValidPair(drag.sourceEl, hit)) {
        hover = hit;
      }
      if (drag.hoverEl && drag.hoverEl !== hover) {
        drag.hoverEl.classList.remove("snap-target");
      }
      drag.hoverEl = hover;
      if (hover) hover.classList.add("snap-target");

      let targetX = e.clientX - svgRect.left;
      let targetY = e.clientY - svgRect.top;
      if (hover) {
        const hp = socketCenter(hover, svgRect);
        targetX = hp.x;
        targetY = hp.y;
      }
      setTemp({ from: p1, to: { x: targetX, y: targetY } });
    }

    function onUp() {
      const drag = dragRef.current;
      if (!drag) return;
      if (drag.hoverEl && isValidPair(drag.sourceEl, drag.hoverEl)) {
        const fromEl = drag.sourceIsRight ? drag.sourceEl : drag.hoverEl;
        const toEl = drag.sourceIsRight ? drag.hoverEl : drag.sourceEl;
        const fromId = fromEl.getAttribute("data-socket-id");
        const toId = toEl.getAttribute("data-socket-id");
        if (fromId && toId) onConnect(fromId, toId);
      }
      if (drag.hoverEl) drag.hoverEl.classList.remove("snap-target");
      dragRef.current = null;
      setTemp(null);
    }

    document.addEventListener("mousemove", onMove);
    document.addEventListener("mouseup", onUp);
    return () => {
      document.removeEventListener("mousemove", onMove);
      document.removeEventListener("mouseup", onUp);
    };
  }, [onConnect, svgRef]);

  return (
    <svg ref={svgRef} className="wires-svg-layer">
      {wires.map((wire) => {
        const fromEl = document.querySelector(
          `[data-socket-id="${CSS.escape(wire.fromSocketId)}"]`,
        );
        const toEl = document.querySelector(
          `[data-socket-id="${CSS.escape(wire.toSocketId)}"]`,
        );
        if (!fromEl || !toEl || !svgRef.current) return null;
        const svgRect = svgRef.current.getBoundingClientRect();
        const p1 = socketCenter(fromEl, svgRect);
        const p2 = socketCenter(toEl, svgRect);
        const d = createBezierPath(p1.x, p1.y, p2.x, p2.y);
        const cls = `wire ${wire.enabled ? "" : "wire-disabled"} ${
          selectedWireId === wire.id ? "selected" : ""
        }`;
        return (
          <path
            key={wire.id}
            className={cls}
            d={d}
            data-wire-id={wire.id}
            onMouseEnter={() => {
              fromEl.classList.add("connected");
              toEl.classList.add("connected");
            }}
            onMouseLeave={() => {
              fromEl.classList.remove("connected");
              toEl.classList.remove("connected");
            }}
          />
        );
      })}

      {temp && (
        <path
          className="temp-wire"
          d={createBezierPath(temp.from.x, temp.from.y, temp.to.x, temp.to.y)}
        />
      )}
    </svg>
  );
}
