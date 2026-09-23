export interface Usage {
  prompt_tokens: number;
  completion_tokens: number;
  total_tokens: number;
}

export interface AppMeta {
  model: string;
  input_per_1m: number;
  output_per_1m: number;
}

export interface ChatMessage {
  role: string;
  content: string | null;
  tool_calls?: unknown;
  tool_call_id?: string | null;
  reasoning_content?: string | null;
}

export interface SessionSummary {
  id: string;
  name: string | null;
  messages: number;
  prompt_tokens: number;
  completion_tokens: number;
  total_tokens: number;
  updated_at: number | null;
  workflow_mode: WorkflowMode;
  quick_mode: QuickMode;
}

export type WorkflowMode = "quick" | "full";
export type QuickMode =
  | "auto"
  | "inspiration"
  | "schedule"
  | "map"
  | "xhs"
  | "ctrip"
  | "knowledge";

export interface SessionDetail {
  id: string;
  name: string | null;
  phase: number;
  workflow_mode: WorkflowMode;
  quick_mode: QuickMode;
  usage: Usage;
  messages: ChatMessage[];
  updated_at: number | null;
}

export type AgentEvent =
  | { Step: { n: number } }
  | { Content: string }
  | { Reasoning: string }
  | { ToolCall: { name: string; args: string } }
  | { ToolResult: string }
  | { PhaseChange: { phase: number } }
  | { Usage: Usage }
  | { Done: string }
  | { Error: string };

export type ServerMessage =
  | { type: "history"; data: SessionDetail }
  | { type: "event"; event: AgentEvent }
  | { type: "error"; message: string }
  | { type: "lagged"; dropped: number };
