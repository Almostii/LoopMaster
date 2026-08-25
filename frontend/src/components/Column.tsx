import type { ReactNode } from "react";

export default function Column({
  title,
  subtitle,
  onAdd,
  addTitle,
  children,
}: {
  title: string;
  subtitle?: string;
  onAdd?: () => void;
  addTitle?: string;
  children: ReactNode;
}) {
  return (
    <div className="topology-col">
      <div className="col-header">
        <div className="col-header-info">
          <div className="col-title">{title}</div>
          {subtitle && <div className="col-subtitle">{subtitle}</div>}
        </div>
        {onAdd && (
          <button
            className="btn-add-node"
            title={addTitle ?? "添加"}
            onClick={onAdd}
          >
            +
          </button>
        )}
      </div>
      <div className="topology-col-list">{children}</div>
    </div>
  );
}
