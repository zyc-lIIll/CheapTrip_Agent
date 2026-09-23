import { FormEvent, useEffect, useState } from "react";
import { Link } from "react-router-dom";

import { adminApi, type AdminSettings, type IntegrationStatus, type LoginTask } from "../../api/admin";
import { ApiError } from "../../lib/http";
import { apiUrl, withDemoToken } from "../../lib/url";

type ModelForm = Omit<AdminSettings, "xhs_enabled" | "hotel_crawler_enabled" | "secrets">;

function errorMessage(reason: unknown): string {
  if (reason instanceof ApiError && reason.message.trim()) return reason.message;
  return "操作失败，请稍后重试。";
}

function modelForm(settings: AdminSettings): ModelForm {
  return {
    model: settings.model,
    llm_base_url: settings.llm_base_url,
    provider: settings.provider,
    reasoning_effort: settings.reasoning_effort,
    temperature: settings.temperature,
    max_tokens: settings.max_tokens,
    connect_timeout_secs: settings.connect_timeout_secs,
    read_timeout_secs: settings.read_timeout_secs,
    input_per_1m: settings.input_per_1m,
    output_per_1m: settings.output_per_1m,
    max_concurrent: settings.max_concurrent,
  };
}

export function AdminSettingsPage() {
  const [settings, setSettings] = useState<AdminSettings | null>(null);
  const [integrations, setIntegrations] = useState<IntegrationStatus[]>([]);
  const [form, setForm] = useState<ModelForm | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [xhsTask, setXhsTask] = useState<LoginTask | null>(null);
  const [xhsBusy, setXhsBusy] = useState(false);

  useEffect(() => {
    void Promise.all([adminApi.settings(), adminApi.integrations()])
      .then(([value, integrationValues]) => {
        setSettings(value);
        setForm(modelForm(value));
        setIntegrations(integrationValues);
      })
      .catch((reason) => setError(errorMessage(reason)))
      .finally(() => setLoading(false));
  }, []);

  useEffect(() => {
    if (!xhsTask || xhsTask.status !== "waiting_for_scan") return;
    const timer = window.setInterval(() => {
      void adminApi.xhsLoginStatus(xhsTask.task_id)
        .then((next) => {
          setXhsTask(next);
          if (next.status === "success") {
            void adminApi.integrations().then(setIntegrations).catch(() => undefined);
            setNotice("小红书登录成功。");
          }
        })
        .catch((reason) => setError(errorMessage(reason)));
    }, 3000);
    return () => window.clearInterval(timer);
  }, [xhsTask]);

  function update<K extends keyof ModelForm>(key: K, value: ModelForm[K]) {
    setForm((current) => current ? { ...current, [key]: value } : current);
  }

  async function save(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!form) return;
    setSaving(true);
    setNotice(null);
    setError(null);
    try {
      const result = await adminApi.updateModel(form);
      setSettings(result);
      setForm(modelForm(result));
      setNotice("设置已保存，重启 cheaptrip 后生效。");
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setSaving(false);
    }
  }

  async function toggleIntegration(item: IntegrationStatus) {
    setError(null);
    try {
      const updated = await adminApi.setIntegrationEnabled(item.kind, !item.desired_enabled);
      setIntegrations((current) => current.map((value) => value.kind === updated.kind ? updated : value));
    } catch (reason) {
      setError(errorMessage(reason));
    }
  }

  async function startXhsLogin() {
    setXhsBusy(true);
    setError(null);
    setNotice(null);
    try {
      setXhsTask(await adminApi.startXhsLogin());
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setXhsBusy(false);
    }
  }

  async function logoutXhs() {
    if (!window.confirm("确定让小红书服务登出当前账号吗？")) return;
    setXhsBusy(true);
    setError(null);
    setNotice(null);
    try {
      const result = await adminApi.logoutXhs();
      setXhsTask(null);
      setNotice(result.message || "小红书已登出。");
      setIntegrations(await adminApi.integrations());
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setXhsBusy(false);
    }
  }

  return (
    <main className="admin-page">
      <header className="admin-page-header">
        <div>
          <div className="auth-kicker">拾光者 · ADMIN</div>
          <h1>系统设置</h1>
          <p>模型、费用和并发参数保存到服务器；修改后需要重启服务。</p>
        </div>
        <div className="admin-header-links">
          <Link className="admin-back" to="/admin/users">用户管理</Link>
          <Link className="admin-back" to="/">返回会话</Link>
        </div>
      </header>

      {error && <p className="admin-error" role="alert">{error}</p>}
      {notice && <p className="admin-notice" role="status">{notice}</p>}

      {loading || !form || !settings ? (
        <section className="admin-card"><p className="admin-muted">正在加载…</p></section>
      ) : (
        <>
          <form className="admin-card settings-form" onSubmit={save}>
            <h2>模型与运行参数</h2>
            <div className="settings-grid">
              <label>模型名称<input value={form.model} onChange={(event) => update("model", event.target.value)} disabled={saving} /></label>
              <label>LLM Base URL<input value={form.llm_base_url} onChange={(event) => update("llm_base_url", event.target.value)} disabled={saving} /></label>
              <label>供应商
                <select value={form.provider} onChange={(event) => update("provider", event.target.value as ModelForm["provider"])} disabled={saving}>
                  <option value="auto">自动识别（GLM 或通用）</option>
                  <option value="glm">GLM</option>
                  <option value="openai_compatible">通用 OpenAI-compatible</option>
                </select>
              </label>
              <label>思考强度
                <select value={form.reasoning_effort} onChange={(event) => update("reasoning_effort", event.target.value as ModelForm["reasoning_effort"])} disabled={saving}>
                  <option value="low">low · 较快</option>
                  <option value="high">high · 平衡</option>
                  <option value="max">max · 最充分</option>
                </select>
                <small className="settings-help">仅 GLM-5.3-Flash 发送此字段；通用适配器会保留选择但不发送。保存后需重启服务。</small>
              </label>
              <label>Temperature<input type="number" min="0" max="2" step="0.1" value={form.temperature} onChange={(event) => update("temperature", Number(event.target.value))} disabled={saving} /></label>
              <label>Max Tokens<input type="number" min="1" max="1000000" value={form.max_tokens} onChange={(event) => update("max_tokens", Number(event.target.value))} disabled={saving} /></label>
              <label>连接超时（秒）<input type="number" min="0" max="600" value={form.connect_timeout_secs} onChange={(event) => update("connect_timeout_secs", Number(event.target.value))} disabled={saving} /></label>
              <label>空闲读取超时（秒）<input type="number" min="0" max="3600" value={form.read_timeout_secs} onChange={(event) => update("read_timeout_secs", Number(event.target.value))} disabled={saving} /></label>
              <label>输入价（每百万 token）<input type="number" min="0" step="0.01" value={form.input_per_1m} onChange={(event) => update("input_per_1m", Number(event.target.value))} disabled={saving} /></label>
              <label>输出价（每百万 token）<input type="number" min="0" step="0.01" value={form.output_per_1m} onChange={(event) => update("output_per_1m", Number(event.target.value))} disabled={saving} /></label>
              <label>最大并发<input type="number" min="1" max="64" value={form.max_concurrent} onChange={(event) => update("max_concurrent", Number(event.target.value))} disabled={saving} /></label>
            </div>
            <button className="auth-submit settings-submit" type="submit" disabled={saving}>
              {saving ? "保存中…" : "保存设置"}
            </button>
          </form>

          <section className="admin-card">
            <h2>密钥状态</h2>
            <p className="admin-muted">网页不会读取、显示或修改任何 API Key。</p>
            <div className="secret-list">
              {settings.secrets.map((secret) => (
                <div className="secret-row" key={secret.name}>
                  <span>{secret.name}</span>
                  <strong className={secret.configured ? "secret-ok" : "secret-missing"}>
                    {secret.configured ? "已配置" : "未配置"}
                  </strong>
                </div>
              ))}
            </div>
          </section>

          <section className="admin-card">
            <h2>外部集成</h2>
            <div className="integration-list">
              {integrations.map((item) => (
                <div className="integration-row" key={item.kind}>
                  <div>
                    <strong>{item.name}</strong>
                    <small>
                      期望：{item.desired_enabled ? "启用" : "禁用"} · 当前：{item.ready ? "可用" : item.effective_enabled ? "已启用但未就绪" : "未启用"}
                      {item.login_state === "logged_in" && " · 已登录"}
                      {item.login_state === "not_logged_in" && " · 未登录"}
                      {item.login_state === "waiting_for_scan" && " · 等待扫码"}
                      {item.login_state === "unknown" && " · 登录状态未知"}
                    </small>
                  </div>
                  <div className="integration-actions">
                    <button type="button" onClick={() => void toggleIntegration(item)} disabled={saving || xhsBusy}>
                      {item.desired_enabled ? "禁用" : "启用"}
                    </button>
                    {item.kind === "xhs" && (
                      <>
                        <button type="button" onClick={() => void startXhsLogin()} disabled={xhsBusy || xhsTask?.status === "waiting_for_scan"}>
                          {xhsBusy ? "处理中…" : "获取二维码"}
                        </button>
                        <button type="button" onClick={() => void logoutXhs()} disabled={xhsBusy}>
                          登出
                        </button>
                      </>
                    )}
                  </div>
                  {item.kind === "xhs" && xhsTask && (
                    <div className="integration-login-task">
                      {xhsTask.qr_available && xhsTask.status === "waiting_for_scan" && (
                        <img
                          className="integration-qr"
                          src={withDemoToken(apiUrl(`/api/admin/integrations/xhs/login/${xhsTask.task_id}/qr`)).toString()}
                          alt="小红书登录二维码"
                        />
                      )}
                      <small>
                        {xhsTask.status === "waiting_for_scan" ? "请使用小红书 App 扫码，页面会自动刷新状态。" : xhsTask.message || `登录任务：${xhsTask.status}`}
                      </small>
                    </div>
                  )}
                </div>
              ))}
            </div>
            <p className="admin-muted">开关写入本机配置，重启 cheaptrip 后生效。小红书支持二维码登录和登出；携程扫码登录将在后续工作包接入。</p>
          </section>
        </>
      )}
    </main>
  );
}
