import type { ReactNode } from "react";

export default function Column({
  title,
  subtitle,
  onAdd,
  addTitle,
  addNode,
  children,
}: {
  title: string;
  subtitle?: string;
  onAdd?: () => void;
  addTitle?: string;
  /** 自定义右上角添加控件（如带下拉菜单的 PickerMenu）。 */
  addNode?: ReactNode;
  children: ReactNode;
}) {
  return (
    <div className="topology-col">
      <div className="col-header">
        <div className="col-header-info">
          <div className="col-title">{title}</div>
          {subtitle && <div className="col-subtitle">{subtitle}</div>}
        </div>
        {addNode ? (
          addNode
        ) : onAdd ? (
          <button
            className="btn-add-node"
            title={addTitle ?? "添加"}
            onClick={onAdd}
          >
            +
          </button>
        ) : null}
      </div>
      <div className="topology-col-list">{children}</div>
    </div>
  );
}
