import { FormEvent, useCallback, useEffect, useState } from "react";
import { Link } from "react-router-dom";

import { adminApi, type AdminUser } from "../../api/admin";
import { useAuth } from "../../app/AuthProvider";
import { ApiError } from "../../lib/http";

type DeleteAction = "permanent" | "transfer";

function errorMessage(reason: unknown): string {
  if (reason instanceof ApiError && reason.message.trim()) return reason.message;
  return "操作失败，请稍后重试。";
}

function dateLabel(epoch: number | null): string {
  if (!epoch) return "从未登录";
  return new Date(epoch * 1000).toLocaleString("zh-CN", { dateStyle: "short", timeStyle: "short" });
}

export function AdminUsersPage() {
  const auth = useAuth();
  const [users, setUsers] = useState<AdminUser[]>([]);
  const [username, setUsername] = useState("");
  const [loading, setLoading] = useState(true);
  const [working, setWorking] = useState<number | "create" | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [temporaryPassword, setTemporaryPassword] = useState<{ username: string; value: string } | null>(null);
  const [deleteTarget, setDeleteTarget] = useState<AdminUser | null>(null);
  const [deleteConfirm, setDeleteConfirm] = useState("");
  const [deleteAction, setDeleteAction] = useState<DeleteAction>("permanent");
  const [transferAdminId, setTransferAdminId] = useState<number | "">("");

  const refresh = useCallback(async () => {
    setUsers(await adminApi.users());
  }, []);

  useEffect(() => {
    void refresh()
      .catch((reason) => setError(errorMessage(reason)))
      .finally(() => setLoading(false));
  }, [refresh]);

  async function create(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const trimmed = username.trim();
    if (!trimmed) {
      setError("请输入用户名。");
      return;
    }
    setWorking("create");
    setError(null);
    try {
      const result = await adminApi.createUser(trimmed);
      setTemporaryPassword({ username: result.user.username, value: result.temporary_password });
      setUsername("");
      await refresh();
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setWorking(null);
    }
  }

  async function update(id: number, input: { enabled?: boolean; role?: AdminUser["role"] }) {
    setWorking(id);
    setError(null);
    try {
      await adminApi.updateUser(id, input);
      await refresh();
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setWorking(null);
    }
  }

  async function resetPassword(user: AdminUser) {
    if (!window.confirm(`确定重置 ${user.username} 的密码吗？旧密码将立即失效。`)) return;
    setWorking(user.id);
    setError(null);
    try {
      const result = await adminApi.resetPassword(user.id);
      setTemporaryPassword({ username: result.user.username, value: result.temporary_password });
      await refresh();
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setWorking(null);
    }
  }

  async function revokeSessions(user: AdminUser) {
    if (!window.confirm(`确定撤销 ${user.username} 的全部登录吗？`)) return;
    setWorking(user.id);
    setError(null);
    try {
      await adminApi.revokeSessions(user.id);
      await refresh();
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setWorking(null);
    }
  }

  function openDelete(user: AdminUser) {
    setDeleteTarget(user);
    setDeleteConfirm("");
    setDeleteAction("permanent");
    setTransferAdminId("");
    setError(null);
  }

  function closeDelete() {
    if (working !== null) return;
    setDeleteTarget(null);
  }

  async function deleteUser() {
    if (!deleteTarget) return;
    if (deleteConfirm !== deleteTarget.username) {
      setError("请输入完全一致的用户名以确认删除。");
      return;
    }
    if (deleteAction === "transfer" && transferAdminId === "") {
      setError("请选择接收数据的管理员。");
      return;
    }
    setWorking(deleteTarget.id);
    setError(null);
    try {
      await adminApi.deleteUser(deleteTarget.id, {
        confirm_username: deleteConfirm,
        data_action: deleteAction,
        ...(deleteAction === "transfer" ? { transfer_to_admin_id: transferAdminId as number } : {}),
      });
      setDeleteTarget(null);
      await refresh();
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setWorking(null);
    }
  }

  return (
    <main className="admin-page">
      <header className="admin-page-header">
        <div>
          <div className="auth-kicker">拾光者 · ADMIN</div>
          <h1>用户管理</h1>
          <p>创建普通用户并管理账号状态。密码只在创建或重置成功时显示一次。</p>
        </div>
        <div className="admin-header-links">
          <Link className="admin-back" to="/admin/settings">系统设置</Link>
          <Link className="admin-back" to="/">返回会话</Link>
        </div>
      </header>

      <section className="admin-card">
        <h2>创建普通用户</h2>
        <form className="admin-create-form" onSubmit={create}>
          <label>
            用户名
            <input
              autoComplete="off"
              value={username}
              onChange={(event) => setUsername(event.target.value)}
              placeholder="例如 alice"
              disabled={working !== null}
            />
          </label>
          <button className="auth-submit" type="submit" disabled={working !== null}>
            {working === "create" ? "创建中…" : "创建用户"}
          </button>
        </form>
        {temporaryPassword && (
          <div className="temporary-password" role="status">
            <strong>{temporaryPassword.username} 的一次性密码</strong>
            <code>{temporaryPassword.value}</code>
            <button
              type="button"
              onClick={() => void navigator.clipboard?.writeText(temporaryPassword.value)}
            >
              复制
            </button>
            <small>请立即交给用户；离开此页面后不会再次显示。</small>
          </div>
        )}
      </section>

      {error && <p className="admin-error" role="alert">{error}</p>}

      <section className="admin-card">
        <div className="admin-section-heading">
          <h2>账号列表</h2>
          <button type="button" onClick={() => void refresh()} disabled={loading || working !== null}>刷新</button>
        </div>
        {loading ? (
          <p className="admin-muted">正在加载…</p>
        ) : users.length === 0 ? (
          <p className="admin-muted">暂无账号。</p>
        ) : (
          <div className="user-list">
            {users.map((user) => {
              const busy = working === user.id;
              const isSelf = auth.user?.id === user.id;
              return (
                <article className="user-row" key={user.id}>
                  <div className="user-identity">
                    <strong>{user.username}</strong>
                    <span>{user.role === "admin" ? "管理员" : "普通用户"} · {user.enabled ? "已启用" : "已禁用"}</span>
                    <small>最后登录：{dateLabel(user.last_login_at)}</small>
                  </div>
                  <div className="user-actions">
                    <select
                      aria-label={`${user.username} 的权限`}
                      value={user.role}
                      disabled={busy || isSelf}
                      onChange={(event) => void update(user.id, { role: event.target.value as AdminUser["role"] })}
                    >
                      <option value="user">普通用户</option>
                      <option value="admin">管理员</option>
                    </select>
                    <button type="button" disabled={busy || isSelf} onClick={() => void update(user.id, { enabled: !user.enabled })}>
                      {user.enabled ? "禁用" : "启用"}
                    </button>
                    <button type="button" disabled={busy} onClick={() => void resetPassword(user)}>重置密码</button>
                    <button type="button" disabled={busy} onClick={() => void revokeSessions(user)}>撤销登录</button>
                    {user.role === "user" && !isSelf && (
                      <button type="button" className="danger-button" disabled={busy} onClick={() => openDelete(user)}>
                        删除用户
                      </button>
                    )}
                  </div>
                </article>
              );
            })}
          </div>
        )}
      </section>

      {deleteTarget && (
        <div className="admin-modal-backdrop" role="presentation" onMouseDown={closeDelete}>
          <section
            className="admin-modal"
            role="dialog"
            aria-modal="true"
            aria-labelledby="delete-user-title"
            onMouseDown={(event) => event.stopPropagation()}
          >
            <h2 id="delete-user-title">删除普通用户</h2>
            <p>
              将删除 <strong>{deleteTarget.username}</strong> 的账号。请选择旅行数据处理方式；操作完成后账号无法恢复。
            </p>
            <label className="admin-choice">
              <input
                type="radio"
                name="delete-action"
                checked={deleteAction === "permanent"}
                onChange={() => setDeleteAction("permanent")}
                disabled={working !== null}
              />
              永久删除全部旅行数据（不可恢复）
            </label>
            <label className="admin-choice">
              <input
                type="radio"
                name="delete-action"
                checked={deleteAction === "transfer"}
                onChange={() => setDeleteAction("transfer")}
                disabled={working !== null}
              />
              转移旅行数据给管理员
            </label>
            {deleteAction === "transfer" && (
              <label className="admin-modal-field">
                接收管理员
                <select
                  value={transferAdminId}
                  onChange={(event) => setTransferAdminId(event.target.value ? Number(event.target.value) : "")}
                  disabled={working !== null}
                >
                  <option value="">请选择</option>
                  {users.filter((user) => user.role === "admin" && user.enabled).map((admin) => (
                    <option value={admin.id} key={admin.id}>{admin.username}</option>
                  ))}
                </select>
              </label>
            )}
            <label className="admin-modal-field">
              输入用户名确认：{deleteTarget.username}
              <input
                value={deleteConfirm}
                onChange={(event) => setDeleteConfirm(event.target.value)}
                autoComplete="off"
                disabled={working !== null}
                placeholder={deleteTarget.username}
              />
            </label>
            <div className="admin-modal-actions">
              <button type="button" onClick={closeDelete} disabled={working !== null}>取消</button>
              <button type="button" className="danger-button" onClick={() => void deleteUser()} disabled={working !== null}>
                {working === deleteTarget.id ? "处理中…" : "确认删除"}
              </button>
            </div>
          </section>
        </div>
      )}
    </main>
  );
}
