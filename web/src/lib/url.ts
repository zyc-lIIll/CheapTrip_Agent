const configuredBase = import.meta.env.VITE_API_BASE_URL?.trim() ?? "";

/**
 * 生产默认同源；免费跨域调试时才通过 VITE_API_BASE_URL 覆盖。
 * 不在业务组件里拼 host，保证以后迁移到 trip.example.com 无需改代码。
 */
export function apiUrl(path: string): URL {
  const normalizedPath = path.startsWith("/") ? path : `/${path}`;
  const base = configuredBase || window.location.origin;
  return new URL(normalizedPath, `${base.replace(/\/$/, "")}/`);
}

/** M0 临时口令兼容层；M2 Cookie 认证完成后删除。 */
export function withDemoToken(url: URL): URL {
  const token = new URLSearchParams(window.location.search).get("token");
  if (token) {
    url.searchParams.set("token", token);
  }
  return url;
}

export function webSocketUrl(path: string): URL {
  const url = withDemoToken(apiUrl(path));
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  return url;
}
