//! Web 层共享状态：会话 → Core 句柄管理。
//!
//! - broadcast 事件扇出：多个 WS 连接（多标签页）各自订阅同一会话的事件流；
//! - 全局并发闸门（[web].max_concurrent）：跨会话限流，chat 时领取许可，
//!   一轮终点事件（Done/Error，一轮恰好一个，见 core.rs/agent.rs）由
//!   forwarder 归还，第 3 个并发请求在闸门上排队；
//! - 打断令牌登记：stop 从这里取当前轮的 CancellationToken。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result, anyhow, bail};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::BuildTools;
use super::auth::AuthBackend;
use crate::agent::{Agent, AgentEvent};
use crate::config::Config;
use crate::core::Core;
use crate::llm;
use crate::session::Session;

/// 单会话运行句柄：输入端 + 事件扇出端 + 打断/并发记账。
pub struct CoreHandle {
    /// agent actor 输入端（等价 Core::submit 的原始通道）
    tx: mpsc::Sender<(String, CancellationToken)>,
    /// 事件扇出：每个 WS 连接各持一个订阅
    pub events: broadcast::Sender<AgentEvent>,
    /// 当前轮的取消令牌（stop 用）
    cancel: Mutex<Option<CancellationToken>>,
    /// 当前轮持有的全局并发许可；forwarder 见 Done/Error 后置 None 归还
    working: Mutex<Option<OwnedSemaphorePermit>>,
    /// agent actor 任务；删除会话时 abort + await，防止删档后被本轮重新保存。
    task: Mutex<Option<JoinHandle<()>>>,
}

/// std Mutex 毒化兜底（仅在持锁 panic 时发生，转为 anyhow 错误而非连锁 panic）。
fn lock<T>(m: &Mutex<T>) -> Result<MutexGuard<'_, T>> {
    m.lock().map_err(|e| anyhow!("互斥锁中毒：{e}"))
}

impl CoreHandle {
    /// 该会话是否有一轮在跑。
    pub fn is_busy(&self) -> bool {
        self.working.lock().map(|w| w.is_some()).unwrap_or(false)
    }

    /// 提交一轮对话。单会话同时只允许一轮（actor 本就串行），
    /// busy 时直接拒绝，避免并发许可记账被排队轮次搅乱。
    pub fn chat(&self, user: String, permit: OwnedSemaphorePermit) -> Result<CancellationToken> {
        {
            let mut working = lock(&self.working)?;
            if working.is_some() {
                bail!("该会话已有对话在进行中");
            }
            *working = Some(permit);
        }
        let cancel = CancellationToken::new();
        if let Err(e) = self.tx.try_send((user, cancel.clone())) {
            // 提交失败：立即归还许可，闸门不留鬼影
            *lock(&self.working)? = None;
            return Err(anyhow!("提交对话失败（会话 agent 已退出）：{e}"));
        }
        *lock(&self.cancel)? = Some(cancel.clone());
        Ok(cancel)
    }

    /// 打断当前轮；返回是否确有一轮被中断。
    pub fn stop(&self) -> Result<bool> {
        let mut cancel = lock(&self.cancel)?;
        Ok(cancel
            .take()
            .map(|t| {
                t.cancel();
                true
            })
            .unwrap_or(false))
    }

    /// 永久关闭该会话 actor。先取消当前轮，再 abort 并等待任务退出。
    async fn shutdown(&self) -> Result<()> {
        let _ = self.stop()?;
        let task = lock(&self.task)?.take();
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
        *lock(&self.working)? = None;
        Ok(())
    }
}

#[derive(Default)]
struct CoreRegistry {
    cores: HashMap<String, Arc<CoreHandle>>,
    deleting: HashSet<String>,
}

/// Router 全局共享状态。
pub struct AppState {
    pub cfg: Config,
    pub client: llm::Client,
    pub auth: Option<Arc<AuthBackend>>,
    build_tools: Arc<BuildTools>,
    /// 全局并发闸门（跨会话）
    pub gate: Arc<Semaphore>,
    /// 活跃会话句柄
    registry: Mutex<CoreRegistry>,
}

impl AppState {
    #[allow(dead_code)]
    pub fn new(cfg: Config, client: llm::Client, build_tools: Arc<BuildTools>) -> Self {
        Self::new_with_auth(cfg, client, build_tools, None)
    }

    pub fn new_with_auth(
        cfg: Config,
        client: llm::Client,
        build_tools: Arc<BuildTools>,
        auth: Option<Arc<AuthBackend>>,
    ) -> Self {
        Self {
            gate: Arc::new(Semaphore::new(cfg.web.max_concurrent.max(1))),
            cfg,
            client,
            auth,
            build_tools,
            registry: Mutex::new(CoreRegistry::default()),
        }
    }

    /// 取会话句柄；首次触碰时加载/新建 session 并启动 agent actor + 事件扇出。
    /// 纯同步（Session 读档/存档、Core::spawn 均无 await），锁不跨 await。
    pub fn ensure_core(&self, sid: &str) -> Result<Arc<CoreHandle>> {
        let mut registry = lock(&self.registry)?;
        if registry.deleting.contains(sid) {
            bail!("会话 {sid} 正在删除");
        }
        if let Some(h) = registry.cores.get(sid) {
            return Ok(h.clone());
        }
        if !Session::exists(sid) {
            bail!("会话 {sid} 不存在，请先通过 REST 创建");
        }
        let session = Session::load(sid).with_context(|| format!("加载会话 {sid} 失败"))?;
        let agent = Agent::new(self.client.clone(), &self.cfg, (self.build_tools)(sid));
        let core = Core::spawn(agent, session);
        let (tx, mut rx, task) = core.into_web_parts();
        let (events, _) = broadcast::channel::<AgentEvent>(256);
        let handle = Arc::new(CoreHandle {
            tx,
            events: events.clone(),
            cancel: Mutex::new(None),
            working: Mutex::new(None),
            task: Mutex::new(Some(task)),
        });

        // 事件扇出 + 轮次终点回收并发许可（Done/Error 是一轮的确定终点）
        let h = handle.clone();
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                if matches!(ev, AgentEvent::Done(_) | AgentEvent::Error(_))
                    && let Ok(mut w) = h.working.lock()
                {
                    *w = None;
                }
                let _ = events.send(ev);
            }
        });
        registry.cores.insert(sid.to_string(), handle.clone());
        Ok(handle)
    }

    /// 删除会话：阻止并发重建，终止 actor 并等待退出后，再删除全部持久化产物。
    pub async fn delete_session(&self, sid: &str) -> Result<()> {
        self.quiesce_session(sid).await?;
        let result = Session::delete(sid);
        self.release_quiesced(sid)?;
        result
    }

    /// 为管理员删除/转移用户数据而暂停一个会话，并阻止 WebSocket 在文件操作期间重建它。
    /// 成功后必须调用 `release_quiesced`；调用方可在多个会话全部暂停后批量处理文件。
    pub async fn quiesce_session(&self, sid: &str) -> Result<()> {
        let handle = {
            let mut registry = lock(&self.registry)?;
            if !registry.deleting.insert(sid.to_string()) {
                bail!("会话 {sid} 正在删除或维护");
            }
            registry.cores.remove(sid)
        };
        if let Some(handle) = handle
            && let Err(error) = handle.shutdown().await
        {
            let _ = self.release_quiesced(sid);
            return Err(error);
        }
        Ok(())
    }

    /// 解除 `quiesce_session` 设置的维护标记，允许后续访问重新加载会话。
    pub fn release_quiesced(&self, sid: &str) -> Result<()> {
        lock(&self.registry)?.deleting.remove(sid);
        Ok(())
    }
}

pub type SharedState = Arc<AppState>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, CostConfig, LlmConfig, SessionConfig, WebConfig};

    fn test_state() -> AppState {
        let cfg = Config {
            llm: LlmConfig {
                base_url: "http://127.0.0.1:9".into(),
                model: "test".into(),
                api_key_env: "CHEAPTRIP_TEST_KEY".into(),
                temperature: 0.7,
                max_tokens: 100,
                connect_timeout_secs: 1,
                read_timeout_secs: 1,
                provider: Default::default(),
                reasoning_effort: Default::default(),
            },
            cost: CostConfig {
                input_per_1m: 0.0,
                output_per_1m: 0.0,
            },
            session: SessionConfig {
                max_messages: None,
                max_sessions: None,
            },
            xhs: Default::default(),
            web: WebConfig::default(),
            auth: Default::default(),
        };
        AppState::new(
            cfg,
            llm::Client::for_test(),
            Arc::new(|_sid: &str| -> Vec<Box<crate::tools::DynTool>> { Vec::new() }),
        )
    }

    /// 即使旧 WebSocket 仍持有 CoreHandle，删除也必须终止 actor，且不能重建已删会话。
    #[tokio::test]
    async fn delete_stops_actor_with_stale_handle() {
        let sid = "test-delete-live-core";
        let _ = Session::delete(sid);
        Session::new(sid.into()).save().unwrap();

        let state = test_state();
        let stale_handle = state.ensure_core(sid).unwrap();
        let permit = state.gate.clone().acquire_owned().await.unwrap();
        stale_handle.chat("hi".into(), permit).unwrap();

        state.delete_session(sid).await.unwrap();
        tokio::task::yield_now().await;

        assert!(!Session::exists(sid), "actor 退出后不得重新保存已删会话");
        assert!(
            state.ensure_core(sid).is_err(),
            "已删 SID 不得由 WS 隐式重建"
        );
        let permit = state.gate.clone().acquire_owned().await.unwrap();
        assert!(
            stale_handle.chat("again".into(), permit).is_err(),
            "旧连接持有的句柄必须失效"
        );
    }
}
