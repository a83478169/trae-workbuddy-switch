//! webui / CLI 形态的 **Trae** 网关宿主：按配置起独立监听（默认 `127.0.0.1:7864`）。
//!
//! 与 [`crate::gateway_host`] 平行、互不复用（原因见 `buddy_switch_gateway::trae` 模块文档）。
//!
//! ## 为什么不把 Trae 路由 merge 进 webui 的 API Router
//!
//! `buddy_switch_gateway::trae::router()` 也挂 `/v1/models` 与 `/v1/chat/completions`，
//! 与 WorkBuddy 网关的路径**逐字相同**。axum 的 `merge` 遇到同路径会直接 panic，
//! 所以 Trae 网关只能走**自己的端口**；webui 侧只暴露 `/api/trae/gateway/*` 管理面。
//!
//! 这不是缺陷而是刻意设计：两个网关的上游、凭据、错误语义完全不同，混在一个端口上
//! 还得靠路径前缀区分，客户端配置会变得难以理解。

use std::sync::OnceLock;

use tokio::sync::Mutex;

use buddy_switch_gateway::trae::{spawn_listener, TraeGatewayState};
use buddy_switch_gateway::GatewayHandle;

static HOST: OnceLock<Mutex<TraeGatewayHost>> = OnceLock::new();

/// 进程内共享的 Trae 网关状态。
///
/// **委托**给 gateway crate 的每进程单例
/// （[`buddy_switch_gateway::trae::process_shared_state`]），不再自建 `OnceLock`：
/// 桌面端在同一进程里同时托管 webui 服务与管理命令，自建会让两侧各持一份
/// [`TraeGatewayState`]，账号池 / Key / 请求日志互不可见。
pub fn shared_state() -> TraeGatewayState {
    buddy_switch_gateway::trae::process_shared_state()
}

/// 独立监听生命周期管理。
#[derive(Default)]
pub struct TraeGatewayHost {
    handle: Option<GatewayHandle>,
}

impl TraeGatewayHost {
    /// 新建（未启动）。
    pub fn new() -> Self {
        Self { handle: None }
    }

    /// 按当前配置启动/重启独立监听。返回实际监听地址；未启用时返回 `None`。
    pub async fn start(&mut self) -> Result<Option<String>, String> {
        self.stop().await;
        let state = shared_state();
        let config = state.config_snapshot().await;
        if !config.enabled {
            return Ok(None);
        }
        let handle = spawn_listener(state)
            .await
            .map_err(|error| error.to_string())?;
        let addr = handle.addr().to_string();
        self.handle = Some(handle);
        Ok(Some(addr))
    }

    /// 停止独立监听。
    pub async fn stop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.shutdown().await;
        }
    }

    /// 是否运行中。
    pub fn is_running(&self) -> bool {
        self.handle.is_some()
    }

    /// 运行中的监听地址。
    pub fn addr(&self) -> Option<String> {
        self.handle.as_ref().map(|handle| handle.addr().to_string())
    }
}

fn host() -> &'static Mutex<TraeGatewayHost> {
    HOST.get_or_init(|| Mutex::new(TraeGatewayHost::new()))
}

/// 按当前配置应用（启动/重启）独立监听。
pub async fn apply() -> Result<Option<String>, String> {
    let mut host = host().lock().await;
    host.start().await
}

/// 查询运行状态（运行中, 监听地址）。
pub async fn status() -> (bool, Option<String>) {
    let host = host().lock().await;
    (host.is_running(), host.addr())
}
