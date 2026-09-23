import { FormEvent, useState } from "react";
import { useLocation, useNavigate } from "react-router-dom";

import { useAuth } from "../app/AuthProvider";
import { ApiError } from "../lib/http";

function messageOf(error: unknown): string {
  if (error instanceof ApiError && error.message.trim()) return error.message;
  return "登录失败，请检查网络后重试。";
}

export function LoginPage() {
  const auth = useAuth();
  const navigate = useNavigate();
  const location = useLocation();
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!username.trim() || !password) {
      setError("请输入用户名和密码。");
      return;
    }
    setError(null);
    setSubmitting(true);
    try {
      const response = await auth.login(username.trim(), password);
      const from = (location.state as { from?: string } | null)?.from;
      navigate(response.user.must_change_password ? "/change-password" : from || "/", {
        replace: true,
      });
    } catch (reason) {
      setError(messageOf(reason));
    } finally {
      setSubmitting(false);
    }
  }

  return (
    <main className="auth-screen">
      <section className="auth-card" aria-labelledby="login-title">
        <div className="auth-kicker">拾光者 · CHEAPTRIP</div>
        <h1 id="login-title">欢迎回来</h1>
        <p className="auth-intro">登录后继续你的旅行规划。</p>
        <form className="auth-form" onSubmit={submit}>
          <label>
            用户名
            <input
              autoComplete="username"
              autoFocus
              value={username}
              onChange={(event) => setUsername(event.target.value)}
              disabled={submitting}
            />
          </label>
          <label>
            密码
            <input
              type="password"
              autoComplete="current-password"
              value={password}
              onChange={(event) => setPassword(event.target.value)}
              disabled={submitting}
            />
          </label>
          {error && <p className="auth-error" role="alert">{error}</p>}
          <button className="auth-submit" type="submit" disabled={submitting}>
            {submitting ? "登录中…" : "登录"}
          </button>
        </form>
        <p className="auth-footnote">账号由管理员创建；如忘记密码，请联系管理员重置。</p>
      </section>
    </main>
  );
}
