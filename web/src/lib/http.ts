import { apiUrl, withDemoToken } from "./url";

export class ApiError extends Error {
  constructor(
    public readonly status: number,
    message: string,
  ) {
    super(message);
    this.name = "ApiError";
  }
}

let csrfToken: string | null = null;

/** 由 AuthProvider 更新；所有写接口自动带上当前账号的 CSRF token。 */
export function setCsrfToken(token: string | null): void {
  csrfToken = token;
}

export async function apiRequest<T>(path: string, init?: RequestInit): Promise<T> {
  const headers = new Headers(init?.headers);
  if (init?.body && !headers.has("content-type")) {
    headers.set("content-type", "application/json");
  }
  const method = (init?.method ?? "GET").toUpperCase();
  if (csrfToken && !["GET", "HEAD", "OPTIONS"].includes(method) && !headers.has("x-csrf-token")) {
    headers.set("x-csrf-token", csrfToken);
  }
  const response = await fetch(withDemoToken(apiUrl(path)), {
    ...init,
    headers,
    // 认证 Cookie 在同源部署和 Vite 跨域开发代理中都要发送。
    credentials: "include",
  });
  if (!response.ok) {
    throw new ApiError(response.status, await response.text());
  }
  if (response.status === 204) {
    return undefined as T;
  }
  return (await response.json()) as T;
}
