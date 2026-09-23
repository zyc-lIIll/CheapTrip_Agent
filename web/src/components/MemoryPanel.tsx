import { lazy, Suspense } from "react";

import { Icon } from "./Icon";

const Markdown = lazy(() => import("./Markdown").then((module) => ({ default: module.Markdown })));

interface Props {
  notes: string | null;
  open: boolean;
  onClose(): void;
}

export function MemoryPanel({ notes, open, onClose }: Props) {
  return (
    <aside className={`sidebar memory-panel ${open ? "is-open" : ""}`}>
      <div className="panel-heading">
        <div>
          <span className="eyebrow">TRIP MEMORY</span>
          <h2>已确认的信息</h2>
        </div>
        <button className="icon-button" onClick={onClose} aria-label="关闭记忆栏">
          <Icon name="close" />
        </button>
      </div>
      <div className="memory-content">
        {notes ? (
          <Suspense fallback={<p>{notes}</p>}>
            <Markdown>{notes}</Markdown>
          </Suspense>
        ) : (
          <div className="empty-memory">
            <Icon name="memory" />
            <strong>还没有形成旅行记忆</strong>
            <p>你确认的目的地、预算和偏好，会在这里逐步沉淀。</p>
          </div>
        )}
      </div>
      <div className="memory-tip">内容由 Agent 在关键确认后自动更新</div>
    </aside>
  );
}
