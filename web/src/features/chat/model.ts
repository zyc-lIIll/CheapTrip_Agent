import type { AgentEvent, ChatMessage, SessionDetail, Usage } from "../../api/types.js";

import { extractMediaPaths } from "./media.js";

export { extractMediaPaths } from "./media.js";

export interface ViewMessage {
  id: string;
  role: "user" | "assistant";
  content: string;
  artifacts?: string[];
  error?: boolean;
}

export interface ToolActivity {
  id: string;
  name: string;
  status: "running" | "done";
}

export interface ChatState {
  messages: ViewMessage[];
  activities: ToolActivity[];
  phase: number;
  usage: Usage;
  busy: boolean;
  reasoning: string;
  currentAssistantId: string | null;
  error: string | null;
  mediaSid: string | null;
}

const EMPTY_USAGE: Usage = {
  prompt_tokens: 0,
  completion_tokens: 0,
  total_tokens: 0,
};

let sequence = 0;
function id(prefix: string): string {
  sequence += 1;
  return `${prefix}-${sequence}`;
}

function historyMessages(messages: ChatMessage[], sid: string): ViewMessage[] {
  const seenArtifacts = new Set<string>();
  return messages.flatMap((message) => {
    if (message.role === "tool") {
      const artifacts = (message.content ? extractMediaPaths(message.content, sid) : [])
        .filter((path) => !seenArtifacts.has(path));
      artifacts.forEach((path) => seenArtifacts.add(path));
      return artifacts.length ? [{ id: id("artifact"), role: "assistant", content: "", artifacts }] : [];
    }
    if ((message.role !== "user" && message.role !== "assistant") || !message.content) {
      return [];
    }
    return [{ id: id("history"), role: message.role, content: message.content }];
  });
}

export const initialChatState: ChatState = {
  messages: [],
  activities: [],
  phase: 0,
  usage: EMPTY_USAGE,
  busy: false,
  reasoning: "",
  currentAssistantId: null,
  error: null,
  mediaSid: null,
};

export type ChatAction =
  | { type: "reset" }
  | { type: "history"; detail: SessionDetail }
  | { type: "user-sent"; text: string }
  | { type: "event"; event: AgentEvent }
  | { type: "transport-error"; message: string };

function appendAssistant(state: ChatState, delta: string): ChatState {
  if (state.currentAssistantId) {
    return {
      ...state,
      messages: state.messages.map((message) =>
        message.id === state.currentAssistantId
          ? { ...message, content: message.content + delta }
          : message,
      ),
    };
  }
  const messageId = id("assistant");
  return {
    ...state,
    currentAssistantId: messageId,
    messages: [...state.messages, { id: messageId, role: "assistant", content: delta }],
  };
}

function finishAssistant(state: ChatState, finalContent: string): ChatState {
  if (!finalContent) {
    return { ...state, busy: false, currentAssistantId: null };
  }
  if (!state.currentAssistantId) {
    return {
      ...state,
      busy: false,
      messages: [
        ...state.messages,
        { id: id("assistant"), role: "assistant", content: finalContent },
      ],
    };
  }
  return {
    ...state,
    busy: false,
    currentAssistantId: null,
    messages: state.messages.map((message) =>
      message.id === state.currentAssistantId
        ? { ...message, content: finalContent }
        : message,
    ),
  };
}

export function chatReducer(state: ChatState, action: ChatAction): ChatState {
  if (action.type === "reset") {
    return initialChatState;
  }
  if (action.type === "history") {
    return {
      ...initialChatState,
      messages: historyMessages(action.detail.messages, action.detail.id),
      phase: action.detail.phase,
      usage: action.detail.usage,
      mediaSid: action.detail.id,
    };
  }
  if (action.type === "user-sent") {
    return {
      ...state,
      busy: true,
      error: null,
      reasoning: "",
      messages: [
        ...state.messages,
        { id: id("user"), role: "user", content: action.text },
      ],
    };
  }
  if (action.type === "transport-error") {
    return { ...state, busy: false, error: action.message, currentAssistantId: null };
  }

  const event = action.event;
  if ("Step" in event) {
    return { ...state, busy: true, currentAssistantId: null, reasoning: "" };
  }
  if ("Content" in event) {
    return appendAssistant(state, event.Content);
  }
  if ("Reasoning" in event) {
    return { ...state, reasoning: state.reasoning + event.Reasoning };
  }
  if ("ToolCall" in event) {
    return {
      ...state,
      currentAssistantId: null,
      activities: [
        ...state.activities.filter((activity) => activity.status === "running"),
        { id: id("tool"), name: event.ToolCall.name, status: "running" },
      ],
    };
  }
  if ("ToolResult" in event) {
    const artifacts = state.mediaSid ? extractMediaPaths(event.ToolResult, state.mediaSid) : [];
    const existing = new Set(state.messages.flatMap((message) => message.artifacts ?? []));
    const fresh = artifacts.filter((path) => !existing.has(path));
    return {
      ...state,
      activities: state.activities.map((activity, index, activities) =>
        index === activities.length - 1 ? { ...activity, status: "done" } : activity,
      ),
      messages: fresh.length
        ? [...state.messages, { id: id("artifact"), role: "assistant", content: "", artifacts: fresh }]
        : state.messages,
    };
  }
  if ("PhaseChange" in event) {
    return { ...state, phase: event.PhaseChange.phase };
  }
  if ("Usage" in event) {
    return { ...state, usage: event.Usage };
  }
  if ("Done" in event) {
    return finishAssistant(state, event.Done);
  }
  return {
    ...state,
    busy: false,
    currentAssistantId: null,
    error: event.Error,
    messages: [
      ...state.messages,
      { id: id("error"), role: "assistant", content: event.Error, error: true },
    ],
  };
}
