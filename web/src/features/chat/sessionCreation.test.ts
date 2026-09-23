import {
  createSessionPayload,
  sessionCreationPayload,
  workflowModeBadge,
  workflowModeLabel,
} from "./sessionCreation.js";

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(message);
}

function equal<T>(actual: T, expected: T, message: string): void {
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {
    throw new Error(`${message}: ${JSON.stringify(actual)} !== ${JSON.stringify(expected)}`);
  }
}

equal(createSessionPayload("quick"), { workflow_mode: "quick" }, "快捷创建请求携带顶层模式");
equal(
  createSessionPayload("full", "  我的完整攻略  "),
  { workflow_mode: "full", name: "我的完整攻略" },
  "完整创建请求携带模式并清理名称",
);
equal(sessionCreationPayload({ type: "cancel" }), null, "取消创建不产生请求 payload");
equal(
  sessionCreationPayload({ type: "confirm", workflowMode: "full" }),
  { workflow_mode: "full" },
  "确认创建才产生请求 payload",
);
assert(workflowModeLabel("quick") === "快捷模式", "快捷模式标签正确");
assert(workflowModeLabel("full") === "完整攻略", "完整攻略标签正确");
assert(workflowModeBadge("quick").readOnly, "模式标签保持只读状态");
assert(workflowModeBadge("full").readOnly, "完整模式标签保持只读状态");

console.log("session creation tests passed");
