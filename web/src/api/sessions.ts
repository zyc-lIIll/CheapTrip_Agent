import { apiRequest } from "../lib/http";
import { sessionCreationPayload } from "../features/chat/sessionCreation";
import type { SessionDetail, SessionSummary, WorkflowMode } from "./types";

export const sessionsApi = {
  list: () => apiRequest<SessionSummary[]>("/api/sessions"),
  detail: (sid: string) => apiRequest<SessionDetail>(`/api/sessions/${sid}`),
  create: (workflowMode: WorkflowMode, name?: string) =>
    apiRequest<SessionDetail>("/api/sessions", {
      method: "POST",
      body: JSON.stringify(sessionCreationPayload({ type: "confirm", workflowMode, name })),
    }),
  rename: (sid: string, name: string) =>
    apiRequest<SessionDetail>(`/api/sessions/${sid}/rename`, {
      method: "POST",
      body: JSON.stringify({ name }),
    }),
  remove: (sid: string) =>
    apiRequest<void>(`/api/sessions/${sid}`, { method: "DELETE" }),
  notes: (sid: string) =>
    apiRequest<{ content: string | null }>(`/api/sessions/${sid}/notes`),
};
