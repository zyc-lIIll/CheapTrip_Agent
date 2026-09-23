import { useEffect, useState } from "react";

import type { WorkflowMode } from "../api/types";
import { workflowModeLabel } from "../features/chat/sessionCreation";

interface Props {
  open: boolean;
  submitting: boolean;
  onCancel(): void;
  onConfirm(mode: WorkflowMode): void;
}

export function SessionCreationDialog({ open, submitting, onCancel, onConfirm }: Props) {
  const [mode, setMode] = useState<WorkflowMode>("quick");

  useEffect(() => {
    if (!open) return;
    setMode("quick");
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !submitting) onCancel();
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [onCancel, open, submitting]);

  if (!open) return null;

  return (
    <div
      className="session-dialog-backdrop"
      role="presentation"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget && !submitting) onCancel();
      }}
    >
      <section
        className="session-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="session-dialog-title"
        aria-describedby="session-dialog-description"
      >
        <h2 id="session-dialog-title">选择旅程模式</h2>
        <p id="session-dialog-description">创建后顶层模式将锁定，不能在会话中途切换。</p>
        <div className="session-mode-options" role="radiogroup" aria-label="旅程模式">
          {(["quick", "full"] as const).map((option) => (
            <label className={`session-mode-option ${mode === option ? "is-selected" : ""}`} key={option}>
              <input
                type="radio"
                name="workflow-mode"
                value={option}
                checked={mode === option}
                onChange={() => setMode(option)}
                autoFocus={option === "quick"}
                disabled={submitting}
              />
              <span>
                <strong>{workflowModeLabel(option)}</strong>
                <small>
                  {option === "quick" ? "一次处理种草、排程、地图等明确任务" : "按阶段整理完整旅行攻略"}
                </small>
              </span>
            </label>
          ))}
        </div>
        <div className="session-dialog-actions">
          <button type="button" className="button-secondary" onClick={onCancel} disabled={submitting}>
            取消
          </button>
          <button type="button" className="button-primary" onClick={() => onConfirm(mode)} disabled={submitting}>
            {submitting ? "创建中…" : "确认创建"}
          </button>
        </div>
      </section>
    </div>
  );
}
