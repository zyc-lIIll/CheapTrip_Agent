//! UI 无关后端核心。任何前端（TUI / Web / 手机）都通过 [`Core`] 接入。
//!
//! 接入契约：
//! 1. [`Core::spawn`] 启动 agent actor（长驻，持有 session），返回 [`Core`]。
//! 2. 输入：[`Core::submit`] 提交用户消息，返回该轮 [`CancellationToken`]；调 `.cancel()` 中止。
//! 3. 输出：从 [`Core::events_mut`] 读 [`AgentEvent`] 流（正文/推理/工具/完成/错误增量）。
//!    `AgentEvent` 实现了 `Serialize`，Web/手机前端可直接 `serde_json::to_string` 经 WebSocket 推送。
//! 4. 新增前端：实现 [`Frontend`] trait，在 `main` 选用即可，无需改动 agent/llm/session。

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, AgentEvent};
use crate::session::Session;

/// 后端核心：持有 agent actor 任务 + 输入/输出 channel 两端。
pub struct Core {
    tx_input: Sender<(String, CancellationToken)>,
    rx_event: Receiver<AgentEvent>,
    task: JoinHandle<()>,
}

impl Core {
    /// 启动 agent actor（长驻，持有 session），返回前端可用的核心句柄。
    pub fn spawn(agent: Agent, session: Session) -> Self {
        let (tx_input, mut rx_input) = mpsc::channel::<(String, CancellationToken)>(16);
        let (tx_event, rx_event) = mpsc::channel::<AgentEvent>(64);
        let task = tokio::spawn(async move {
            let mut session = session;
            while let Some((user, cancel)) = rx_input.recv().await {
                if let Err(e) = agent
                    .run(&mut session, &user, tx_event.clone(), cancel)
                    .await
                {
                    // {e:#} 输出完整 anyhow 错误链（顶层 + 底层原因），便于定位网络层问题
                    let _ = tx_event.send(AgentEvent::Error(format!("{e:#}"))).await;
                }
                // 每轮跑完：更新最后对话时间 + 立即存档
                session.updated_at = Some(std::time::SystemTime::now());
                let _ = session.save();
            }
        });
        Self {
            tx_input,
            rx_event,
            task,
        }
    }

    /// 提交一轮用户消息，返回该轮的取消令牌（调 `.cancel()` 中止生成）。
    /// 作为新前端的高层接入契约保留；当前 TUI/Web 使用下方拆分通道接口。
    #[allow(dead_code)]
    pub fn submit(&self, user: String) -> CancellationToken {
        let cancel = CancellationToken::new();
        let _ = self.tx_input.try_send((user, cancel.clone()));
        cancel
    }

    /// 事件流接收端（前端消费增量）。
    /// 作为新前端的高层接入契约保留；当前 TUI/Web 使用下方拆分通道接口。
    #[allow(dead_code)]
    pub fn events_mut(&mut self) -> &mut Receiver<AgentEvent> {
        &mut self.rx_event
    }

    /// 拆成裸 channel 两端（前端直接持有时用；如当前 TUI）。
    pub fn into_parts(self) -> (Sender<(String, CancellationToken)>, Receiver<AgentEvent>) {
        (self.tx_input, self.rx_event)
    }

    /// Web 会话管理需要持有 actor 任务，以便删除会话时可靠终止并等待退出。
    pub fn into_web_parts(
        self,
    ) -> (
        Sender<(String, CancellationToken)>,
        Receiver<AgentEvent>,
        JoinHandle<()>,
    ) {
        (self.tx_input, self.rx_event, self.task)
    }
}

/// 前端接口。新增 Web/手机前端时实现此 trait，在 `main` 选用。
///
/// 返回 `true` 表示要切回会话选择页（TUI 用），`false` 表示正常退出。
#[async_trait]
pub trait Frontend: Sized {
    async fn run(self, core: Core) -> Result<bool>;
}
