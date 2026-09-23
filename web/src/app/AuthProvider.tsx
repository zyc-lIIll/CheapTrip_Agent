import { createContext, useCallback, useContext, useEffect, useMemo, useState } from "react";
import type { ReactNode } from "react";
import { Navigate, useLocation } from "react-router-dom";

import { ApiError, setCsrfToken as setHttpCsrfToken } from "../lib/http";
import { authApi, type AuthResponse, type CurrentUser } from "../api/auth";

type AuthStatus = "loading" | "disabled" | "anonymous" | "authenticated";

interface AuthContextValue {
  status: AuthStatus;
  user: CurrentUser | null;
  csrfToken: string | null;
  login(username: string, password: string): Promise<AuthResponse>;
  logout(): Promise<void>;
  changePassword(password: string, currentPassword?: string): Promise<AuthResponse>;
  refresh(): Promise<void>;
}

const AuthContext = createContext<AuthContextValue | null>(null);

function isAuthDisabled(error: unknown): boolean {
  // 认证关闭时 /api/auth/me 不存在；静态 fallback 还可能返回 HTML，
  // 此时 JSON 解析会抛 SyntaxError，也应回到 M0 的公开演示模式。
  return (error instanceof ApiError && error.status === 404) || error instanceof SyntaxError;
}

export function AuthProvider({ children }: { children: ReactNode }) {
  const [status, setStatus] = useState<AuthStatus>("loading");
  const [user, setUser] = useState<CurrentUser | null>(null);
  const [csrfToken, setCsrfToken] = useState<string | null>(null);

  const applyResponse = useCallback((response: AuthResponse) => {
    setUser(response.user);
    setCsrfToken(response.csrf_token);
    setHttpCsrfToken(response.csrf_token);
    setStatus("authenticated");
  }, []);

  const refresh = useCallback(async () => {
    try {
      applyResponse(await authApi.currentUser());
    } catch (error) {
      setUser(null);
      setCsrfToken(null);
      setHttpCsrfToken(null);
      setStatus(isAuthDisabled(error) ? "disabled" : "anonymous");
    }
  }, [applyResponse]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const login = useCallback(
    async (username: string, password: string) => {
      const response = await authApi.login(username, password);
      applyResponse(response);
      return response;
    },
    [applyResponse],
  );

  const logout = useCallback(async () => {
    if (csrfToken) {
      await authApi.logout(csrfToken);
    }
    setUser(null);
    setCsrfToken(null);
    setHttpCsrfToken(null);
    setStatus("anonymous");
  }, [csrfToken]);

  const changePassword = useCallback(
    async (password: string, currentPassword?: string) => {
      if (!csrfToken) {
        throw new ApiError(401, "尚未登录");
      }
      const response = await authApi.changePassword(password, csrfToken, currentPassword);
      applyResponse(response);
      return response;
    },
    [applyResponse, csrfToken],
  );

  const value = useMemo(
    () => ({ status, user, csrfToken, login, logout, changePassword, refresh }),
    [status, user, csrfToken, login, logout, changePassword, refresh],
  );

  return <AuthContext.Provider value={value}>{children}</AuthContext.Provider>;
}

export function useAuth(): AuthContextValue {
  const context = useContext(AuthContext);
  if (!context) {
    throw new Error("useAuth 必须在 AuthProvider 内使用");
  }
  return context;
}

function Loading() {
  return <main className="auth-screen"><div className="auth-card" aria-busy="true">正在检查登录状态…</div></main>;
}

export function ProtectedRoute({ children }: { children: ReactNode }) {
  const auth = useAuth();
  const location = useLocation();
  if (auth.status === "loading") return <Loading />;
  if (auth.status === "disabled") return <>{children}</>;
  if (auth.status === "anonymous") {
    return <NavigateToLogin />;
  }
  if (auth.user?.must_change_password && location.pathname !== "/change-password") {
    return <NavigateToChangePassword />;
  }
  return <>{children}</>;
}

export function LoginRoute({ children }: { children: ReactNode }) {
  const auth = useAuth();
  if (auth.status === "loading") return <Loading />;
  if (auth.status === "disabled") return <NavigateToHome />;
  if (auth.status === "authenticated") {
    return auth.user?.must_change_password ? <NavigateToChangePassword /> : <NavigateToHome />;
  }
  return <>{children}</>;
}

export function ChangePasswordRoute({ children }: { children: ReactNode }) {
  const auth = useAuth();
  if (auth.status === "loading") return <Loading />;
  if (auth.status === "disabled") return <NavigateToHome />;
  if (auth.status === "anonymous") return <NavigateToLogin />;
  return <>{children}</>;
}

export function AdminRoute({ children }: { children: ReactNode }) {
  const auth = useAuth();
  if (auth.status === "disabled") return <>{children}</>;
  if (auth.user?.role !== "admin") {
    return <main className="auth-screen"><div className="auth-card"><h1>没有权限</h1><p>该页面仅管理员可用。</p></div></main>;
  }
  return <>{children}</>;
}

function NavigateToLogin() {
  const location = useLocation();
  return <Navigate replace to="/login" state={{ from: location.pathname }} />;
}

function NavigateToChangePassword() {
  return <Navigate replace to="/change-password" />;
}

function NavigateToHome() {
  return <Navigate replace to="/" />;
}
