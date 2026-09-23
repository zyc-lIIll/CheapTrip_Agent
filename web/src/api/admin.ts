import { apiRequest } from "../lib/http";
import type { UserRole } from "./auth";

export interface SecretStatus {
  name: string;
  configured: boolean;
}

export interface AdminSettings {
  model: string;
  llm_base_url: string;
  provider: "auto" | "glm" | "openai_compatible";
  reasoning_effort: "low" | "high" | "max";
  temperature: number;
  max_tokens: number;
  connect_timeout_secs: number;
  read_timeout_secs: number;
  input_per_1m: number;
  output_per_1m: number;
  max_concurrent: number;
  xhs_enabled: boolean;
  hotel_crawler_enabled: boolean;
  secrets: SecretStatus[];
}

export interface ModelSettingsResponse extends AdminSettings {
  restart_required: boolean;
}

export interface IntegrationStatus {
  kind: "xhs" | "hotel";
  name: string;
  configured_enabled: boolean;
  desired_enabled: boolean;
  effective_enabled: boolean;
  login_state: "unknown" | "logged_in" | "not_logged_in" | "waiting_for_scan";
  ready: boolean;
  note: string;
}

export interface LoginTask {
  task_id: string;
  status: "waiting_for_scan" | "success" | "failed" | "expired" | "cancelled";
  message: string | null;
  expires_at: number;
  qr_available: boolean;
}

export interface AdminUser {
  id: number;
  username: string;
  role: UserRole;
  enabled: boolean;
  must_change_password: boolean;
  last_login_at: number | null;
  created_at: number;
  updated_at: number;
}

export interface TemporaryPasswordResponse {
  user: AdminUser;
  temporary_password: string;
}

export interface DeleteUserResponse {
  user: AdminUser;
  deleted_sessions: number;
  transferred_sessions: number;
}

/** M2 目标协议；真实密钥永远不得出现在响应类型里。 */
export const adminApi = {
  settings: () => apiRequest<AdminSettings>("/api/admin/settings"),
  updateModel: (input: Omit<AdminSettings, "xhs_enabled" | "hotel_crawler_enabled" | "secrets">) =>
    apiRequest<ModelSettingsResponse>("/api/admin/settings/model", {
      method: "PUT",
      body: JSON.stringify(input),
    }),
  integrations: () => apiRequest<IntegrationStatus[]>("/api/admin/integrations"),
  setIntegrationEnabled: (kind: IntegrationStatus["kind"], enabled: boolean) =>
    apiRequest<IntegrationStatus>(`/api/admin/integrations/${kind}/enabled`, {
      method: "PUT",
      body: JSON.stringify({ enabled }),
    }),
  startXhsLogin: () => apiRequest<LoginTask>("/api/admin/integrations/xhs/login/start", { method: "POST" }),
  xhsLoginStatus: (taskId: string) => apiRequest<LoginTask>(`/api/admin/integrations/xhs/login/${taskId}`),
  logoutXhs: () => apiRequest<{ ok: boolean; message: string }>("/api/admin/integrations/xhs/logout", { method: "POST" }),
  users: () => apiRequest<AdminUser[]>("/api/admin/users"),
  createUser: (username: string) =>
    apiRequest<TemporaryPasswordResponse>("/api/admin/users", {
      method: "POST",
      body: JSON.stringify({ username }),
    }),
  updateUser: (id: number, input: { enabled?: boolean; role?: UserRole }) =>
    apiRequest<AdminUser>(`/api/admin/users/${id}`, {
      method: "PATCH",
      body: JSON.stringify(input),
    }),
  resetPassword: (id: number) =>
    apiRequest<TemporaryPasswordResponse>(`/api/admin/users/${id}/reset-password`, {
      method: "POST",
    }),
  revokeSessions: (id: number) =>
    apiRequest<AdminUser>(`/api/admin/users/${id}/revoke-sessions`, {
      method: "POST",
    }),
  deleteUser: (
    id: number,
    input: { confirm_username: string; data_action: "permanent" | "transfer"; transfer_to_admin_id?: number },
  ) =>
    apiRequest<DeleteUserResponse>(`/api/admin/users/${id}/delete`, {
      method: "POST",
      body: JSON.stringify(input),
    }),
};
