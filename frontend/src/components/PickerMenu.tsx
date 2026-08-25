import { useEffect, useRef, useState, type ReactNode } from "react";

export interface PickerOption {
  value: string;
  label: string;
  hint?: string;
}

export default function PickerMenu({
  title,
  trigger,
  options,
  onSelect,
  onOpen,
  loading,
}: {
  title: string;
  trigger: ReactNode;
  options: PickerOption[];
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

  return (
    <div className="picker-root" ref={rootRef}>
      <div onClick={toggle}>{trigger}</div>
      {open && (
        <div className="dropdown-menu show">
          <div className="dropdown-section-title">{title}</div>
          {loading ? (
            <div className="dropdown-item muted">加载中…</div>
          ) : options.length === 0 ? (
            <div className="dropdown-item muted">暂无可用项</div>
          ) : (
            options.map((opt) => (
              <div
                key={opt.value}
                className="dropdown-item"
                onClick={() => {
                  onSelect(opt.value);
                  setOpen(false);
                }}
              >
                <span className="dropdown-item-label">{opt.label}</span>
                {opt.hint && <span className="dropdown-item-hint">{opt.hint}</span>}
              </div>
            ))
          )}
        </div>
      )}
    </div>
  );
}
