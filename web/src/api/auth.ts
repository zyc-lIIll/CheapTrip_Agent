import { apiRequest } from "../lib/http";

export type UserRole = "admin" | "user";

export interface CurrentUser {
  id: number;
  username: string;
  role: UserRole;
  must_change_password: boolean;
}

export interface AuthResponse {
  user: CurrentUser;
  csrf_token: string;
}

export const authApi = {
  login: (username: string, password: string) =>
    apiRequest<AuthResponse>("/api/auth/login", {
      method: "POST",
      body: JSON.stringify({ username, password }),
    }),
  logout: (csrfToken: string) =>
    apiRequest<void>("/api/auth/logout", {
      method: "POST",
      headers: { "X-CSRF-Token": csrfToken },
    }),
  currentUser: () => apiRequest<AuthResponse>("/api/auth/me"),
  changePassword: (password: string, csrfToken: string, currentPassword?: string) =>
    apiRequest<AuthResponse>("/api/auth/change-password", {
      method: "POST",
      headers: { "X-CSRF-Token": csrfToken },
      body: JSON.stringify({
        password,
        ...(currentPassword ? { current_password: currentPassword } : {}),
      }),
    }),
};
