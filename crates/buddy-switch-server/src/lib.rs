//! `buddy-switch-server` 的库入口。
//!
//! 本 crate 原先只有二进制（`buddy-switch`）。抽成 lib + bin 是为了让**桌面端**
//! （`src-tauri`）能复用同一份 webui 服务：`api::router()` 与两个网关宿主都是
//! 框架无关的（不依赖 Tauri），桌面端在自己的进程里
//! `axum::serve(listener, api::router())` 即可得到与 `buddy-switch serve` 完全
//! 相同的 webui —— 两条入口共享同一份前端产物与同一份业务状态。
//!
//! 二进制入口仍是 `src/main.rs`，行为与改造前一致。

pub mod api;
pub mod gateway_host;
pub mod trae_gateway_host;
