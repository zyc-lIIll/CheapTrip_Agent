import { FormEvent, useState } from "react";
import { useNavigate } from "react-router-dom";

import { useAuth } from "../app/AuthProvider";
import { ApiError } from "../lib/http";

function messageOf(error: unknown): string {
  if (error instanceof ApiError && error.message.trim()) return error.message;
  return "修改密码失败，请稍后重试。";
}

export function ChangePasswordPage() {
  const auth = useAuth();
  const navigate = useNavigate();
  const [password, setPassword] = useState("");
  const [currentPassword, setCurrentPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (password.length < 8) {
      setError("新密码至少需要 8 个字符。");
      return;
    }
    if (!auth.user?.must_change_password && !currentPassword) {
      setError("请输入当前密码。");
      return;
    }
    if (password !== confirm) {
      setError("两次输入的密码不一致。");
      return;
    }
    setError(null);
    setSubmitting(true);
    try {
      await auth.changePassword(password, currentPassword || undefined);
      navigate("/", { replace: true });
    } catch (reason) {
      setError(messageOf(reason));
    } finally {
      setSubmitting(false);
    }
  }

  return (
    <main className="auth-screen">
      <section className="auth-card" aria-labelledby="change-password-title">
        <div className="auth-kicker">{auth.user?.must_change_password ? "首次登录" : "账号设置"}</div>
        <h1 id="change-password-title">设置新密码</h1>
        <p className="auth-intro">
          {auth.user?.must_change_password
            ? "当前密码是一次性临时密码，请先设置一个仅你知道的新密码。"
            : "输入当前密码后设置一个新密码。"}
        </p>
        <form className="auth-form" onSubmit={submit}>
          {!auth.user?.must_change_password && (
            <label>
              当前密码
              <input
                type="password"
                autoComplete="current-password"
                autoFocus
                value={currentPassword}
                onChange={(event) => setCurrentPassword(event.target.value)}
                disabled={submitting}
              />
            </label>
          )}
          <label>
            新密码
            <input
              type="password"
              autoComplete="new-password"
              autoFocus={Boolean(auth.user?.must_change_password)}
              minLength={8}
              value={password}
              onChange={(event) => setPassword(event.target.value)}
              disabled={submitting}
            />
          </label>
          <label>
            再输入一次
            <input
              type="password"
              autoComplete="new-password"
              value={confirm}
              onChange={(event) => setConfirm(event.target.value)}
              disabled={submitting}
            />
          </label>
          {error && <p className="auth-error" role="alert">{error}</p>}
          <button className="auth-submit" type="submit" disabled={submitting}>
            {submitting ? "保存中…" : "保存新密码"}
          </button>
        </form>
      </section>
    </main>
  );
}
