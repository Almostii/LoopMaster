import type { ReactNode } from "react";

/**
 * 侧边栏菜单项描述。
 * icon 元素需在 16x16 视口下绘制, 在收起态仍清晰可辨。
 */
export interface SidebarItem {
  key: string;
  label: string;
  icon: ReactNode;
}

export interface SidebarGroup {
  /** 可选小标题(只在展开态显示) */
  title?: string;
  items: SidebarItem[];
}

export default function Sidebar({
  collapsed,
  activeKey,
  onSelect,
  topItems,
  bottomItems,
}: {
  collapsed: boolean;
  activeKey: string;
  onSelect: (key: string) => void;
  /** 顶部主菜单项(首页/路由/分析等) */
  topItems: SidebarItem[];
  /** 底部辅助菜单项(设置等, 固定在侧边栏底部) */
  bottomItems?: SidebarItem[];
}) {
  return (
    <aside className={`sidebar ${collapsed ? "is-collapsed" : ""}`}>
      <nav className="sidebar-nav">
        <ul className="sidebar-list">
          {topItems.map((item) => {
            const active = item.key === activeKey;
            return (
              <li key={item.key}>
                <button
                  type="button"
                  className={`sidebar-item ${active ? "is-active" : ""}`}
                  onClick={() => onSelect(item.key)}
                  title={collapsed ? item.label : undefined}
                >
                  <span className="sidebar-item-icon">{item.icon}</span>
                  {!collapsed && (
                    <span className="sidebar-item-label">{item.label}</span>
                  )}
                </button>
              </li>
            );
          })}
        </ul>
      </nav>

      {bottomItems && bottomItems.length > 0 && (
        <nav className="sidebar-footer-nav">
          <ul className="sidebar-list">
            {bottomItems.map((item) => {
              const active = item.key === activeKey;
              return (
                <li key={item.key}>
                  <button
                    type="button"
                    className={`sidebar-item ${active ? "is-active" : ""}`}
                    onClick={() => onSelect(item.key)}
                    title={collapsed ? item.label : undefined}
                  >
                    <span className="sidebar-item-icon">{item.icon}</span>
                    {!collapsed && (
                      <span className="sidebar-item-label">{item.label}</span>
                    )}
                  </button>
                </li>
              );
            })}
          </ul>
        </nav>
      )}
    </aside>
  );
}
