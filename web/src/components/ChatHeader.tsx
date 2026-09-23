import type { AppMeta, SessionSummary, Usage, WorkflowMode } from "../api/types";
import type { CurrentUser } from "../api/auth";
import { Link } from "react-router-dom";
import { Icon } from "./Icon";
import { workflowModeBadge } from "../features/chat/sessionCreation";

interface Props {
  session: SessionSummary | null;
  phase: number;
  usage: Usage;
  cost: number | null;
  meta: AppMeta | null;
  connection: "disconnected" | "connecting" | "connected";
  theme: "light" | "dark";
  onMenu(): void;
  onMemory(): void;
  workflowMode: WorkflowMode;
  onTheme(): void;
  onReconnect(): void;
  user: CurrentUser | null;
  onLogout(): void;
}

const PHASES = ["种草闲聊", "信息采集", "大局规划", "逐区确定", "整体调整", "完整攻略"];

export function ChatHeader(props: Props) {
  const modeBadge = workflowModeBadge(props.workflowMode);
  const connectionLabel = {
    connected: "已连接",
    connecting: "连接中",
    disconnected: "未连接",
  }[props.connection];

  return (
    <header className="chat-header">
      <button className="icon-button mobile-only" onClick={props.onMenu} aria-label="打开会话栏">
        <Icon name="menu" />
      </button>
      <div className="chat-title">
        <span className="eyebrow">PHASE {props.phase}</span>
        <h1>{props.session?.name || "新的旅程"}</h1>
        <div className="header-meta">
          <button className="connection" onClick={props.onReconnect} title="重新连接">
            <span className={`connection-dot ${props.connection}`} />
            {connectionLabel}
          </button>
          <span>{PHASES[props.phase] ?? "规划中"}</span>
          <span>{props.meta?.model ?? "Agent"}</span>
        </div>
      </div>
      <span className="workflow-mode-badge" aria-label={`当前工作流：${modeBadge.label}`} aria-readonly={modeBadge.readOnly}>
        {modeBadge.label}
      </span>
      <div className="header-stats">
        <span>{props.usage.total_tokens.toLocaleString()} tokens</span>
        {props.cost !== null && <span>¥{props.cost.toFixed(4)}</span>}
      </div>
      {props.user && (
        <div className="header-account-group">
          {props.user.role === "admin" && <Link className="header-admin-link" to="/admin/users">管理</Link>}
          <Link className="header-admin-link" to="/change-password">改密</Link>
          <button className="header-account" onClick={props.onLogout} title="退出登录">
            <span>{props.user.username}</span>
            <span>退出</span>
          </button>
        </div>
      )}
      <button className="icon-button" onClick={props.onTheme} aria-label="切换主题">
        <Icon name={props.theme === "light" ? "moon" : "sun"} />
      </button>
      <button className="icon-button" onClick={props.onMemory} aria-label="打开记忆栏">
        <Icon name="memory" />
      </button>
    </header>
  );
}
