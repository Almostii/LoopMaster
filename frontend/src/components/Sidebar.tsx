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
  onToggle,
  activeKey,
  onSelect,
  topItems,
  bottomItems,
  brandLogo,
  brandText,
  user,
  onUserClick,
}: {
  collapsed: boolean;
  onToggle: () => void;
  activeKey: string;
  onSelect: (key: string) => void;
  /** 顶部主菜单项(首页/路由/分析等) */
  topItems: SidebarItem[];
  /** 底部辅助菜单项(报错核对/应用锁等) */
  bottomItems?: SidebarItem[];
  brandLogo?: ReactNode;
  brandText?: string;
  user?: {
    name: string;
    sub?: string;
    avatar?: ReactNode;
  };
  onUserClick?: () => void;
}) {
  return (
    <aside className={`sidebar ${collapsed ? "is-collapsed" : ""}`}>
      <div className="sidebar-brand">
        <div className="sidebar-brand-icon">
          {brandLogo ?? <span className="sidebar-brand-dot" />}
        </div>
        {!collapsed && brandText && (
          <span className="sidebar-brand-text">{brandText}</span>
        )}
        <button
          type="button"
          className="sidebar-toggle"
          onClick={onToggle}
          aria-label={collapsed ? "展开侧边栏" : "收起侧边栏"}
          title={collapsed ? "展开" : "收起"}
        >
          <svg
            width="14"
            height="14"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="2"
            strokeLinecap="round"
            strokeLinejoin="round"
          >
            {collapsed ? (
              <polyline points="9 18 15 12 9 6" />
            ) : (
              <polyline points="15 18 9 12 15 6" />
            )}
          </svg>
        </button>
      </div>

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

      <div className="sidebar-footer">
        {bottomItems && bottomItems.length > 0 && (
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
        )}

        {user && (
          <button
            type="button"
            className="sidebar-user"
            onClick={onUserClick}
            title={collapsed ? user.name : undefined}
          >
            <span className="sidebar-user-avatar">
              {user.avatar ?? <span className="sidebar-user-fallback" />}
            </span>
            {!collapsed && (
              <span className="sidebar-user-info">
                <span className="sidebar-user-name">{user.name}</span>
                {user.sub && (
                  <span className="sidebar-user-sub">{user.sub}</span>
                )}
              </span>
            )}
            {!collapsed && (
              <svg
                className="sidebar-user-chevron"
                width="12"
                height="12"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                strokeWidth="2"
                strokeLinecap="round"
                strokeLinejoin="round"
              >
                <polyline points="6 9 12 15 18 9" />
              </svg>
            )}
          </button>
        )}
      </div>
    </aside>
  );
}
