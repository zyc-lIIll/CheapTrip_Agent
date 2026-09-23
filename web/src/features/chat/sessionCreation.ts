import type { WorkflowMode } from "../../api/types.js";

export function createSessionPayload(workflowMode: WorkflowMode, name?: string): {
  workflow_mode: WorkflowMode;
  name?: string;
} {
  const trimmed = name?.trim();
  return trimmed ? { workflow_mode: workflowMode, name: trimmed } : { workflow_mode: workflowMode };
}

export type SessionCreationIntent =
  | { type: "cancel" }
  | { type: "confirm"; workflowMode: WorkflowMode; name?: string };

export function sessionCreationPayload(intent: SessionCreationIntent): {
  workflow_mode: WorkflowMode;
  name?: string;
} | null {
  return intent.type === "cancel"
    ? null
    : createSessionPayload(intent.workflowMode, intent.name);
}

export function workflowModeLabel(workflowMode: WorkflowMode): string {
  return workflowMode === "full" ? "完整攻略" : "快捷模式";
}

export function workflowModeBadge(workflowMode: WorkflowMode): {
  label: string;
  readOnly: true;
} {
  return { label: workflowModeLabel(workflowMode), readOnly: true };
}
