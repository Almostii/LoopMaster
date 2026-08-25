import { useEffect, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";

export interface PickerOption {
  value: string;
  label: string;
  hint?: string;
  icon?: ReactNode;
}

export interface PickerGroup {
  title: string;
  options: PickerOption[];
}

export default function PickerMenu({
  title,
  trigger,
  options,
  groups,
  onSelect,
  onOpen,
  loading,
}: {
  title: string;
  trigger: ReactNode;
  /** 扁平选项（与 groups 二选一）。 */
  options?: PickerOption[];
  /** 分组选项；传入时按组渲染，每组单独显示标题。 */
  groups?: PickerGroup[];
  onSelect: (value: string) => void;
  onOpen?: () => void;
  loading?: boolean;
}) {
  const [open, setOpen] = useState(false);
  const [menuStyle, setMenuStyle] = useState<{ top: number; left: number }>({
    top: 0,
    left: 0,
  });
  const rootRef = useRef<HTMLDivElement>(null);
  const triggerRef = useRef<HTMLDivElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    function onClickOutside(e: MouseEvent) {
      const target = e.target as Node;
      if (
        triggerRef.current?.contains(target) ||
        menuRef.current?.contains(target)
      ) {
        return;
      }
      setOpen(false);
    }
    document.addEventListener("mousedown", onClickOutside);
    return () => document.removeEventListener("mousedown", onClickOutside);
  }, []);

  useEffect(() => {
    function onResize() {
      if (!open) return;
      setOpen(false);
    }
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }, [open]);

  useEffect(() => {
    if (!open) return;
    function onScroll(e: Event) {
      if (
        e.target instanceof Node &&
        (triggerRef.current?.contains(e.target) ||
          menuRef.current?.contains(e.target))
      ) {
        return;
      }
      setOpen(false);
    }
    window.addEventListener("scroll", onScroll, true);
    return () => window.removeEventListener("scroll", onScroll, true);
  }, [open]);

  function computePosition() {
    const rect = triggerRef.current?.getBoundingClientRect();
    if (!rect) return;
    const menuWidth = Math.min(360, window.innerWidth - 32);
    let left = rect.left;
    if (left + menuWidth > window.innerWidth - 16) {
      left = Math.max(16, window.innerWidth - menuWidth - 16);
    }
    setMenuStyle({ top: rect.bottom + 4, left });
  }

  function toggle() {
    const next = !open;
    if (next) {
      computePosition();
      if (onOpen) onOpen();
    }
    setOpen(next);
  }

  const resolvedGroups: PickerGroup[] = groups ?? [
    { title, options: options ?? [] },
  ];
  const allEmpty = resolvedGroups.every((g) => g.options.length === 0);

  function renderOption(opt: PickerOption) {
    return (
      <div
        key={opt.value}
        className="dropdown-item"
        onClick={() => {
          onSelect(opt.value);
          setOpen(false);
        }}
      >
        {opt.icon ? (
          <span className="dropdown-item-icon">{opt.icon}</span>
        ) : (
          <span className="dropdown-item-icon dropdown-item-icon-placeholder" />
        )}
        <span className="dropdown-item-label">{opt.label}</span>
        {opt.hint && <span className="dropdown-item-hint">{opt.hint}</span>}
      </div>
    );
  }

  const menu = open && (
    <div
      className="dropdown-menu show"
      ref={menuRef}
      style={{
        position: "fixed",
        top: menuStyle.top,
        left: menuStyle.left,
        right: "auto",
      }}
    >
      {loading ? (
        <div className="dropdown-item muted">加载中…</div>
      ) : allEmpty ? (
        <div className="dropdown-item muted">暂无可用项</div>
      ) : (
        resolvedGroups.map((group, i) => (
          <div key={group.title} className="dropdown-group">
            {i > 0 && <div className="dropdown-divider" />}
            <div className="dropdown-section-title">{group.title}</div>
            {group.options.length === 0 ? (
              <div className="dropdown-item muted">—</div>
            ) : (
              group.options.map(renderOption)
            )}
          </div>
        ))
      )}
    </div>
  );

  return (
    <div className="picker-root" ref={rootRef}>
      <div ref={triggerRef} onClick={toggle}>
        {trigger}
      </div>
      {menu && createPortal(menu, document.body)}
    </div>
  );
}
