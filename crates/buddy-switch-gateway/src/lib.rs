//! `buddy-switch-gateway`：WorkBuddy 模型额度的 OpenAI / Anthropic 兼容网关。
//!
//! 本 crate 是网关本体，落在独立 crate（不污染 `buddy-switch-core` 的依赖面），
//! 可被两种宿主同时复用：
//! - `buddy-switch-server`：webui/CLI 形态，`api.rs` 内 `merge(router(state))`，
//!   并可用 [`spawn_listener`] 起独立监听（默认 `127.0.0.1:57891`）。
//! - `src-tauri`：桌面宿主，`gateway.rs` 持有 [`GatewayHandle`]。
//!
//! 对外契约（已冻结，不得更改签名）：
//! ```ignore
//! pub fn router(state: GatewayState) -> axum::Router;
//! pub async fn spawn_listener(config: GatewayConfig) -> anyhow::Result<GatewayHandle>;
//! ```
//!
//! 关键约束：
//! - `router()` **不设 fallback**，避免与宿主 merge 时的 fallback 冲突 panic。
//! - SSE 透传用 `Body::from_stream`；客户端断开即 drop stream → abort 上游。
//! - 上游流中途异常时**补发 `data: [DONE]`**，绝不悬挂连接。

pub mod account_strategy;
pub mod apikey;
pub mod credits_refresh;
pub mod error;
pub mod logging;
pub mod model_route;
pub mod outbound;
pub mod pool;
pub mod protocol;
pub mod rng;
pub mod routes;
pub mod session_headers;
pub mod state;
pub mod sticky;
pub mod timeutil;
/// Trae 的 OpenAI 兼容网关（**平行第二套实现**，与上面的 WorkBuddy 网关互不复用）。
///
/// 独立模块而非 `GatewayState` 的一个分支：两者归属域、凭据形态、上游协议、
/// 响应格式、路由挂载方式全不相同，详见 [`trae`] 模块文档。
pub mod trae;

pub use account_strategy::{AccountSelector, AccountStrategy};
pub use apikey::{ApiKeyRecord, ApiKeyStore};
pub use error::GatewayError;
pub use outbound::{OutboundMeta, OutboundOptions};
pub use rng::Pcg32;
pub use state::{
    body_limit_bytes, sync_pool_with_accounts, GatewayConfig, GatewayState, GatewayStatusView,
    DEFAULT_MAX_BODY_MB,
};
pub use sticky::StickyTable;

use std::net::{IpAddr, SocketAddr};
use std::sync::OnceLock;

use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// 进程内共享的 WorkBuddy 网关状态（**每个进程一份**，惰性初始化，读一次配置）。
///
/// **为什么下沉到这里**：此前的形态是每个宿主各持一份 `OnceLock`
/// （`buddy-switch-server` 的 `gateway_host.rs` 与 `src-tauri` 的 `gateway.rs`
/// 各一个 `static`）。两个独立进程时各一份毫无问题；但桌面端现在会在**同一个进程**
/// 里同时托管 webui 服务与管理命令，两份 `static` 就意味着两份 [`GatewayState`] ——
/// 网页端改了网关配置 / Key，桌面窗口看不到（反之亦然）。
/// 下沉到本 crate 后，同一进程内的所有宿主取到的是同一份。
///
/// 独立进程（`buddy-switch-server` 单独跑）行为不变：进程内依旧只有这一份。
pub fn process_shared_state() -> GatewayState {
    static SHARED: OnceLock<GatewayState> = OnceLock::new();
    SHARED
        .get_or_init(|| GatewayState::new(GatewayConfig::load()))
        .clone()
}

/// 组装网关对外路由（**不含 fallback**）。
///
/// 挂载：`GET /healthz`、`GET /v1/models`、`GET /status`、
/// `POST /v1/chat/completions`、`POST /v1/messages`。
/// 鉴权在各自 handler 内完成（Key 绑定 region）；`/healthz` 免鉴权。
pub fn router(state: GatewayState) -> axum::Router {
    use axum::routing::{get, post};
    // 请求体上限：axum 的 `DefaultBodyLimit` **默认只有 2MB**，大上下文（长代码文件、
    // 长对话）会撞上一个裸 413。它在 **router 构建期**固化，因此改配置需重启网关
    // （见 `GatewayConfig::max_body_mb`）。
    let body_limit = state.body_limit_bytes;
    axum::Router::new()
        .route("/healthz", get(routes::health::handler))
        .route("/v1/models", get(routes::models::handler))
        .route("/status", get(routes::status::handler))
        .route("/v1/chat/completions", post(routes::chat::handler))
        .route("/v1/messages", post(routes::messages::handler))
        .layer(axum::extract::DefaultBodyLimit::max(body_limit))
        .with_state(state)
    // 注意：不设 fallback，避免与宿主 merge 时的 fallback 冲突
}

/// 已启动的网关监听句柄。
///
/// Drop 该句柄（不调用 [`GatewayHandle::shutdown`]）会触发优雅停机：
/// 内部 oneshot sender 被丢弃 → 优雅关闭 future 完成。
pub struct GatewayHandle {
    addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    join: Option<tokio::task::JoinHandle<()>>,
}

impl GatewayHandle {
    /// 由监听方直接构造句柄。
    ///
    /// 字段是私有的，因此**平级模块**（如 `trae::spawn_listener`）无法用结构体字面量
    /// 构造。与其把字段放宽成 `pub(crate)`（那会让「谁都能改 addr」成为可能），
    /// 不如只开一个受控构造器：语义与 [`spawn_listener_with_state`] 内部那处完全一致。
    pub fn new(
        addr: SocketAddr,
        shutdown: oneshot::Sender<()>,
        join: tokio::task::JoinHandle<()>,
    ) -> Self {
        Self {
            addr,
            shutdown: Some(shutdown),
            join: Some(join),
        }
    }

    /// 实际监听地址（端口为 0 时返回系统分配的端口）。
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// 优雅停机：通知 serve 循环退出并等待任务结束。
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.await;
        }
    }
}

/// 按配置启动独立监听（**自建一份** [`GatewayState`]）。
///
/// 向后兼容的旧签名：内部转调 [`spawn_listener_with_state`]。宿主若希望独立监听与
/// 管理面共享同一份状态（请求日志 / Key / 目录缓存），应改用 [`spawn_listener_with_state`]。
///
/// - 默认只监听回环地址；非回环地址需要 `allow_non_loopback = true`。
/// - 端口占用/地址非法时返回**可读错误**（不是 panic）。
pub async fn spawn_listener(config: GatewayConfig) -> anyhow::Result<GatewayHandle> {
    spawn_listener_with_state(GatewayState::new(config)).await
}

/// 用**调用方提供的** [`GatewayState`] 启动独立监听。
///
/// 关键：`state` 是可 `Arc` 共享的句柄（配置 / Key 库 / 目录缓存 / 请求日志均为 `Arc`），
/// 因此独立监听与宿主管理面**看到的是同一份**请求日志与模型目录刷新结果——否则
/// 独立监听会把请求写进它自己的日志，管理页恒为空。
///
/// 监听地址 / 端口 / 是否允许非回环均取自 `state` 内的配置快照。
pub async fn spawn_listener_with_state(state: GatewayState) -> anyhow::Result<GatewayHandle> {
    let config = state.config_snapshot().await;
    let bind_addr = config.bind_addr.clone();
    let port = config.port;

    let ip: IpAddr = bind_addr
        .parse()
        .map_err(|_| anyhow::anyhow!("网关监听地址无效：{bind_addr}（应为 IPv4/IPv6 字面量）"))?;

    if !ip.is_loopback() && !config.allow_non_loopback {
        return Err(anyhow::anyhow!(
            "已拒绝监听非回环地址 {bind_addr}：局域网内任何设备都可消耗你的额度，\
             请确认可信网络后，在设置中显式开启『允许局域网访问』再重试"
        ));
    }

    let addr = SocketAddr::new(ip, port);
    let listener = TcpListener::bind(addr).await.map_err(|error| {
        anyhow::anyhow!("无法监听 {addr}：端口可能被占用或不可用（{error}）。请在设置中改用其他端口。")
    })?;
    let local = listener.local_addr().unwrap_or(addr);

    let app = router(state);

    let (tx, rx) = oneshot::channel::<()>();
    let join = tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await;
    });

    Ok(GatewayHandle {
        addr: local,
        shutdown: Some(tx),
        join: Some(join),
    })
}
