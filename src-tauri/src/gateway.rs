//! 桌面宿主（Tauri）的网关接入：持有独立监听句柄 + 提供共享状态。
//!
//! - [`shared_state`]：进程内共享的 [`GatewayState`]（配置/Key/日志一致）。
//! - [`GatewayRuntime`]：由 `app.manage(...)` 托管，负责独立监听生命周期。
//!   独立监听与命令层复用**同一份** [`shared_state`]（E4），因此桌面端发出到
//!   57891 的真实请求日志 / 目录刷新结果都能在管理页看到。

use std::sync::Mutex;

use buddy_switch_gateway::{spawn_listener_with_state, GatewayHandle, GatewayState};

/// 进程内共享的网关状态。
///
/// **委托**给 gateway crate 的每进程单例（[`buddy_switch_gateway::process_shared_state`]），
/// 不再自建 `OnceLock`：桌面端在同一进程里同时托管 webui 服务与管理命令，
/// 自建会让网页端与桌面端各持一份 [`GatewayState`]，配置 / Key 互不可见。
pub fn shared_state() -> GatewayState {
    buddy_switch_gateway::process_shared_state()
}

/// 独立监听生命周期（Tauri managed state）。
#[derive(Default)]
pub struct GatewayRuntime {
    handle: Mutex<Option<GatewayHandle>>,
}

impl GatewayRuntime {
    /// 新建（未启动）。
    pub fn new() -> Self {
        Self {
            handle: Mutex::new(None),
        }
    }

    /// 按当前配置启动/重启独立监听。返回实际监听地址；未启用时返回 `None`。
    ///
    /// **共享状态（E4）**：用 [`shared_state`] 起监听，使独立监听与管理面共享同一份
    /// 请求日志 / Key 库 / 目录缓存（`save_gateway_config` 会把配置同步写回该状态）。
    pub async fn apply(&self) -> Result<Option<String>, String> {
        // 先停掉旧监听（内部会在锁外 await，避免跨 await 持有 std 锁）。
        self.stop().await;

        let state = shared_state();
        // 以共享状态内的配置为准（保存配置时会同步写回该状态）。
        let config = state.config_snapshot().await;
        if !config.enabled {
            return Ok(None);
        }
        let handle = spawn_listener_with_state(state)
            .await
            .map_err(|error| error.to_string())?;
        let addr = handle.addr().to_string();
        {
            let mut guard = self.handle.lock().unwrap();
            *guard = Some(handle);
        }
        Ok(Some(addr))
    }

    /// 停止独立监听。
    pub async fn stop(&self) {
        let existing = {
            let mut guard = self.handle.lock().unwrap();
            guard.take()
        };
        if let Some(handle) = existing {
            handle.shutdown().await;
        }
    }

    /// 是否运行中。
    pub fn is_running(&self) -> bool {
        self.handle.lock().unwrap().is_some()
    }

    /// 运行中的监听地址。
    pub fn addr(&self) -> Option<String> {
        self.handle
            .lock()
            .unwrap()
            .as_ref()
            .map(|handle| handle.addr().to_string())
    }
}
