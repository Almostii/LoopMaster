import { useEffect, useRef, useState, type ReactNode } from "react";

export interface PickerOption {
  value: string;
  label: string;
  hint?: string;
  icon?: string | null;
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
  const rootRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    function onClickOutside(e: MouseEvent) {
      if (rootRef.current && !rootRef.current.contains(e.target as Node)) {
        setOpen(false);
      }
    }
    document.addEventListener("mousedown", onClickOutside);
    return () => document.removeEventListener("mousedown", onClickOutside);
  }, []);

  function toggle() {
    const next = !open;
    setOpen(next);
    if (next && onOpen) onOpen();
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
          <img className="dropdown-item-icon" src={opt.icon} alt="" />
        ) : (
          <span className="dropdown-item-icon dropdown-item-icon-placeholder" />
        )}
        <span className="dropdown-item-label">{opt.label}</span>
        {opt.hint && <span className="dropdown-item-hint">{opt.hint}</span>}
      </div>
    );
  }

  return (
    <div className="picker-root" ref={rootRef}>
      <div onClick={toggle}>{trigger}</div>
      {open && (
        <div className="dropdown-menu show">
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
      )}
    </div>
  );
}
