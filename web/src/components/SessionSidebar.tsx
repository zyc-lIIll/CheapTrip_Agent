import type { SessionSummary } from "../api/types";
import { Icon } from "./Icon";

interface Props {
  sessions: SessionSummary[];
  selectedId: string | null;
  open: boolean;
  collapsed: boolean;
  loading: boolean;
  onClose(): void;
  onToggleCollapsed(): void;
  onCreate(): void;
  onSelect(sid: string): void;
  onDelete(sid: string): void;
  onRename(sid: string, name: string): void;
}

function relativeTime(epoch: number | null): string {
  if (!epoch) return "刚刚";
  const seconds = Math.max(0, Math.floor(Date.now() / 1000) - epoch);
  if (seconds < 60) return "刚刚";
  if (seconds < 3600) return `${Math.floor(seconds / 60)} 分钟前`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)} 小时前`;
  return `${Math.floor(seconds / 86400)} 天前`;
}

export function SessionSidebar(props: Props) {
  const rename = (session: SessionSummary) => {
    const next = window.prompt("修改会话名称", session.name ?? "");
    if (next?.trim()) props.onRename(session.id, next.trim());
  };
  const remove = (session: SessionSummary) => {
    if (window.confirm(`确定删除“${session.name || "未命名旅程"}”吗？相关地图和记忆也会删除。`)) {
      props.onDelete(session.id);
    }
  };

  return (
    <aside className={`sidebar session-sidebar ${props.open ? "is-open" : ""} ${props.collapsed ? "is-collapsed" : ""}`}>
      <div className="sidebar-heading">
        <div className="brand-mark">拾</div>
        <div>
          <strong>拾光者</strong>
          <span>把向往变成路线</span>
        </div>
        <button
          className="icon-button sidebar-collapse"
          onClick={props.onToggleCollapsed}
          aria-label={props.collapsed ? "展开会话栏" : "折叠会话栏"}
          aria-expanded={!props.collapsed}
        >
          <span aria-hidden="true">{props.collapsed ? "›" : "‹"}</span>
        </button>
        <button className="icon-button mobile-only" onClick={props.onClose} aria-label="关闭会话栏">
          <Icon name="close" />
        </button>
      </div>

      <button className="new-session" onClick={props.onCreate} aria-label="开启一段新旅程">
        <Icon name="add" />
        <span className="new-session-label">开启一段新旅程</span>
      </button>

      <div className="sidebar-label">最近旅程</div>
      <nav className="session-list" aria-label="会话列表">
        {props.loading && (
          <div className="session-skeleton" aria-label="正在加载会话">
            <span />
            <span />
            <span />
          </div>
        )}
        {!props.loading && props.sessions.length === 0 && (
          <div className="sidebar-empty">还没有旅程，先聊聊想去哪儿吧。</div>
        )}
        {props.sessions.map((session) => (
          <div
            className={`session-item ${session.id === props.selectedId ? "is-active" : ""}`}
            key={session.id}
          >
            <button
              className="session-select"
              aria-label={session.name || "未命名旅程"}
              title={session.name || "未命名旅程"}
              onClick={() => {
                props.onSelect(session.id);
                props.onClose();
              }}
            >
              <span>{session.name || "未命名旅程"}</span>
              <span className="session-short" aria-hidden="true">
                {(session.name || "未").trim().slice(0, 1) || "未"}
              </span>
              <small>{relativeTime(session.updated_at)}</small>
            </button>
            <div className="session-actions">
              <button onClick={() => rename(session)} aria-label="重命名">
                <Icon name="edit" />
              </button>
              <button onClick={() => remove(session)} aria-label="删除">
                <Icon name="delete" />
              </button>
            </div>
          </div>
        ))}
      </nav>
      <div className="sidebar-footer">旅行不是赶路，是把日子过成风景。</div>
    </aside>
  );
}
