//! 桌面宿主（Tauri）内置的 **WebUI** 服务宿主。
//!
//! 桌面端与 webui 本质是同一份前端 + 同一套业务，只是通道不同（Tauri IPC / HTTP）。
//! 本模块让桌面进程**同时**监听一个本地 HTTP 端口，于是同一个 exe 既可以在窗口里
//! 操作，也可以在浏览器打开 `http://127.0.0.1:<port>` 使用，且**操作的是同一份数据**
//! （共享状态见 `buddy_switch_gateway::process_shared_state`）。
//!
//! ## 为什么要记录「实际端口」
//!
//! 默认端口 [`DEFAULT_PORT`] 可能被占用；占用时本模块会**递增重试**。因此
//! 「请求的端口」未必是「最终监听的端口」——只把请求值显示给用户，浏览器会打不开。
//! 实际端口记进 [`WebuiRuntime`]，由 `commands::get_webui_info` 与「应用级设置」页展示。

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Mutex;

use tauri::Manager;

/// 默认监听端口（与独立运行的 `buddy-switch serve` 保持一致）。
pub const DEFAULT_PORT: u16 = 57890;

/// 默认端口被占用时，最多向上尝试多少个端口。
///
/// 16 个足够覆盖「被其它程序零星占用若干端口」的常见情形，又不会因为某个端口区间
/// 被整段占用而长时间空转。
const PORT_RETRY_LIMIT: u16 = 16;

/// WebUI 服务的运行状态（Tauri managed state）。
///
/// 与 [`crate::gateway::GatewayRuntime`] / [`crate::trae_gateway::TraeGatewayRuntime`]
/// 同构：用 `Mutex` 满足 Tauri managed state 的 `Send + Sync` 要求。
#[derive(Default)]
pub struct WebuiRuntime {
    port: Mutex<Option<u16>>,
}

impl WebuiRuntime {
    /// 新建（尚未绑定端口）。
    pub fn new() -> Self {
        Self {
            port: Mutex::new(None),
        }
    }

    /// 记录**实际监听**的端口（服务启动成功后调用）。
    pub fn set_port(&self, port: u16) {
        *self.port.lock().unwrap() = Some(port);
    }

    /// 实际监听端口；未启动时为 `None`。
    pub fn port(&self) -> Option<u16> {
        *self.port.lock().unwrap()
    }
}

/// 启动内置 webui 服务：绑定回环端口 → 记录实际端口 → 起 axum。
///
/// 本函数**永不返回**（除非服务异常退出或端口全部不可用），因此调用方应放进
/// `tauri::async_runtime::spawn`。路由直接复用 server crate 的 `api::router()` ——
/// 与独立运行 `buddy-switch serve` 是**同一份**实现，两条入口行为完全一致。
pub async fn serve(app: tauri::AppHandle) -> Result<(), String> {
    let listener = bind_with_retry().await?;
    let addr = listener
        .local_addr()
        .map_err(|error| format!("读取 webui 监听地址失败: {error}"))?;
    {
        // State 只在写入这一刻借用 app，避免跨 await 持有。
        let runtime = app.state::<WebuiRuntime>();
        runtime.set_port(addr.port());
    }
    // 同步给 server crate：浏览器侧 `GET /api/webui/info` 读的就是它。
    buddy_switch_server::api::set_bound_port(addr.port());
    eprintln!("[webui] 已启动: http://{addr}");
    axum::serve(listener, buddy_switch_server::api::router())
        .await
        .map_err(|error| format!("webui 服务退出: {error}"))
}

/// 从 [`DEFAULT_PORT`] 起绑定回环端口，被占用则逐 +1 重试。
async fn bind_with_retry() -> Result<tokio::net::TcpListener, String> {
    let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let mut last_error = String::new();
    for offset in 0..PORT_RETRY_LIMIT {
        let addr = SocketAddr::new(ip, DEFAULT_PORT + offset);
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => return Ok(listener),
            Err(error) => last_error = error.to_string(),
        }
    }
    Err(format!(
        "无法绑定 {DEFAULT_PORT}..={} 之间的回环端口（最后一次错误：{last_error}）",
        DEFAULT_PORT + PORT_RETRY_LIMIT - 1
    ))
}
