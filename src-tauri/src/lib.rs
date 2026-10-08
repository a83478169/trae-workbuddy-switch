// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
mod commands;
mod gateway;
mod trae_gateway;
mod webui_host;
#[cfg(desktop)]
mod tray;

use std::time::Duration;
use tauri::Manager;
use buddy_switch_core::modules;

const SCREENSHOT_DEMO_ENV: &str = "BUDDY_SWITCH_SCREENSHOT_DEMO";

/// WorkBuddy 账号池状态落盘周期（毫秒）。
///
/// 与 `buddy-switch-server` 同值。取值理由：[`Pool::flush_if_dirty`] 只在**有改动**时
/// 才真正写盘，所以这里定的是「最坏情况下最多丢多少治理状态」。30 秒足够短
/// （冷却 / 熔断的时效是分钟级，丢 30 秒不会造成错误决策），又远长于一次请求。
///
/// [`Pool::flush_if_dirty`]: buddy_switch_gateway::pool::Pool::flush_if_dirty
const POOL_PERSIST_INTERVAL_MS: u64 = 30_000;

/// Trae 积分刷新周期（毫秒）。
///
/// 与 WorkBuddy 余额刷新的缺省间隔（30 分钟）保持一致：同一管理面不该有两种节奏。
const TRAE_CREDITS_REFRESH_INTERVAL_MS: u64 = 30 * 60 * 1000;

pub(crate) fn is_screenshot_demo() -> bool {
    std::env::var(SCREENSHOT_DEMO_ENV).as_deref() == Ok("1")
}

/// 桌面端后台任务：**唯一事实来源**。
///
/// ⚠️ 回归背景（本表存在的原因）：此前 `spawn_background_loops` 直接写死四个循环
/// （签到 30 分钟 / 旅行 30 分钟 / 领取 15 分钟 / 保活每天一次），**完全没有排程**，
/// 于是设置页「定时任务排程」的六类开关与六份小时表在桌面端全部空转——「活跃地图」
/// 从未执行过一次。改成注册表后，「增删后台任务」是数据变化，且有测试守住
/// （见 `tests::background_tasks_cover_all_scheduled_tasks`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackgroundTask {
    /// 启动补跑：整理历史签到日志 + 签到核验 / 旅行派出领取 / 保活各跑一轮
    /// （**受各自排程开关约束**，关掉的任务一次都不跑）。
    StartupMaintenance,
    /// 自动轮换（按 `auto_rotate_config` 间隔；CodeBuddy CLI 为 CN 专有）。
    AutoRotate,
    /// 六类积分定时任务之一（按 `schedule_config` 的小时表，各自独立排程）。
    Scheduled(modules::schedule::ScheduleTask),
    /// WorkBuddy 账号池**余额刷新**（真实余额回填选号权重）。
    ///
    /// ★ 桌面端此前**没有**它（只有服务端有），后果是池里的 `credits` 恒为 0 ——
    /// `weight_of` 的积分分支（权重最大的一项）静默退化为死代码，
    /// 对外宣称的「四因子加权」实际只剩「闲置补偿 + 成功率 + 成本分层」。
    CreditsRefresh,
    /// WorkBuddy 账号池**治理状态落盘**（冷却 / 熔断 / 成功率 EMA / 余额读数）。
    ///
    /// 缺了它这些状态只在内存里，每次重启从零开始 —— 例如刚判定 `SessionDead`
    /// 的账号重启后立刻又被选中、再撞一次同样的墙。
    PoolPersist,
    /// **Trae** 积分刷新（两条国内产品线，顺带自动解冻已恢复的账号）。
    ///
    /// Trae 的剩余积分此前只在「签到页」被刷新 —— 纯用 API（不开界面）时读数会一直
    /// 陈旧：已耗尽积分的账号仍被选中（白撞一次上游），或尚有余额的账号被误判为
    /// 零积分而排除。
    TraeCreditsRefresh,
}

/// 后台任务注册表：**登记即执行**——不要在本表之外直接 `spawn` 后台循环。
fn background_tasks() -> Vec<BackgroundTask> {
    let mut tasks = vec![
        BackgroundTask::StartupMaintenance,
        BackgroundTask::AutoRotate,
        BackgroundTask::CreditsRefresh,
        BackgroundTask::PoolPersist,
        BackgroundTask::TraeCreditsRefresh,
    ];
    tasks.extend(
        modules::schedule::ScheduleTask::all()
            .into_iter()
            .map(BackgroundTask::Scheduled),
    );
    tasks
}

/// 启动全部后台任务（以 [`background_tasks`] 为唯一事实来源）。
fn spawn_background_loops() {
    for task in background_tasks() {
        spawn_background_task(task);
    }
}

/// 按注册表条目派生对应的后台循环。
fn spawn_background_task(task: BackgroundTask) {
    match task {
        // 启动补跑：排程小时表之外的「今天该做但还没做」的一次性动作。
        // 语义与服务端 `buddy-switch-server` 完全一致（共用 core::scheduler）。
        BackgroundTask::StartupMaintenance => {
            tauri::async_runtime::spawn(async move {
                modules::scheduler::run_startup_maintenance().await;
            });
        }
        // 自动轮换（CodeBuddy CLI）：按配置间隔执行。
        BackgroundTask::AutoRotate => {
            tauri::async_runtime::spawn(async move {
                let mut last_rotate_at: i64 = 0;
                loop {
                    let rotate_cfg = modules::config::load_auto_rotate_config();
                    if rotate_cfg.get("enabled").and_then(|v| v.as_bool()) == Some(true) {
                        let interval_minutes = rotate_cfg
                            .get("check_interval_minutes")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(5)
                            .max(1);
                        let now = modules::config::now_ms();
                        if now - last_rotate_at >= interval_minutes * 60_000 {
                            last_rotate_at = now;
                            let _ = modules::rotate::run_rotate_cycle().await;
                        }
                    }
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
            });
        }
        // 六类定时任务各自独立排程：每类一个循环，按自己的小时表 sleep 到点。
        // 排程语义在 core::scheduler 里，桌面端与服务端共用，**不在此处另写一份**。
        BackgroundTask::Scheduled(task) => {
            tauri::async_runtime::spawn(modules::scheduler::schedule_loop(task));
        }
        // WorkBuddy 账号池余额刷新：**独立**循环，首次启动先跑一次，之后按池配置
        // `credits_refresh_interval_ms`（默认 30 分钟）周期刷新。语义与服务端同源。
        //
        // 为什么必须是独立后台循环：余额是慢变数据，在请求路径上同步拉余额会直接拉高
        // 每次请求的延迟（见 `buddy_switch_gateway::credits_refresh` 的文档）。
        BackgroundTask::CreditsRefresh => {
            tauri::async_runtime::spawn(async move {
                let state = gateway::shared_state();
                loop {
                    // 启动即刷一次（循环首轮）：补齐上次进程遗留的「从未取过余额」账号。
                    let _ = buddy_switch_gateway::credits_refresh::refresh_once(&state).await;
                    // ★ 与 server 侧**同一判据**（`next_refresh_wait_ms`）：
                    //   池为空 ⇒ 短探测，否则「刚导入账号」要等满一整个周期才看得到积分。
                    let wait_ms = {
                        let pool = state.pool.read().await;
                        buddy_switch_gateway::credits_refresh::next_refresh_wait_ms(&pool)
                    };
                    tokio::time::sleep(Duration::from_millis(wait_ms)).await;
                }
            });
        }
        // WorkBuddy 账号池治理状态落盘（只在有改动时真正写盘）。
        BackgroundTask::PoolPersist => {
            tauri::async_runtime::spawn(async move {
                let state = gateway::shared_state();
                loop {
                    tokio::time::sleep(Duration::from_millis(POOL_PERSIST_INTERVAL_MS)).await;
                    state.persist_pool().await;
                }
            });
        }
        // Trae 积分刷新（两条国内产品线）。
        BackgroundTask::TraeCreditsRefresh => {
            tauri::async_runtime::spawn(async move {
                // 启动即刷一次：补齐上次进程遗留的陈旧读数。
                refresh_trae_credits().await;
                loop {
                    tokio::time::sleep(Duration::from_millis(TRAE_CREDITS_REFRESH_INTERVAL_MS))
                        .await;
                    refresh_trae_credits().await;
                }
            });
        }
    }
}

/// 刷新 Trae **两条国内产品线**的全部账号积分（顺带自动解冻已恢复的账号）。
///
/// 只刷 `TraeVariant::all()`（= `TraeWork` / `Trae`）：网关的 Key 只归属这两条线
/// （见 `buddy_switch_gateway::trae::apikey` 的 `variant`），刷国际版属于无效功。
///
/// 失败不中断：`refresh_all_remaining_for` 内部已对**单账号**失败做了记录与跳过，
/// 这里再兜一层是为了「一个变体整体报错不影响另一个变体」。
async fn refresh_trae_credits() {
    for variant in modules::trae::variant::TraeVariant::all() {
        let _ = modules::trae::credits::refresh_all_remaining_for(variant).await;
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let mut builder = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init());

    #[cfg(desktop)]
    {
        builder = builder.plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec![tray::SILENT_STARTUP_ARG]),
        ));
        builder = builder.on_window_event(tray::on_window_event);
    }

    let app = builder
        .setup(|app| {
            #[cfg(desktop)]
            {
                tray::setup(app)?;
                // 主窗口由配置创建为不可见；在事件循环呈现前决定本次启动是否静默。
                // 仅系统自启（精确 `--hidden` 参数）进入静默托盘，普通启动立即显示主窗口。
                tray::setup_startup_visibility(
                    app.handle(),
                    tray::is_silent_startup(std::env::args()),
                );
            }
            // 网关运行时句柄（管理命令依赖它查询/切换独立监听）。
            app.manage(gateway::GatewayRuntime::new());
            // Trae 网关运行时句柄（与上面那份**互不相干**：配置 / Key / 日志全独立）。
            app.manage(trae_gateway::TraeGatewayRuntime::new());
            // 内置 webui 的运行时句柄（记录**实际**监听端口，供设置页展示）。
            app.manage(webui_host::WebuiRuntime::new());
            // README 截图模式只渲染前端虚构数据，禁止读取账号后执行签到、轮换或保活。
            if !is_screenshot_demo() {
                spawn_background_loops();
                // 内置 webui：桌面进程**同时**在本地监听一个 HTTP 端口，浏览器打开即可用
                // 同一份界面操作同一份数据（见 `webui_host` 模块文档）。与下面两个网关
                // 不同，它是**默认常开**的 —— 它是「一个 exe 两种入口」的第二种入口。
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(error) = webui_host::serve(handle).await {
                        eprintln!("[webui] {error}");
                    }
                });
                // 按配置启动 API 网关独立监听（默认关闭；默认 127.0.0.1:57891）。
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    let runtime = handle.state::<gateway::GatewayRuntime>();
                    match runtime.apply().await {
                        Ok(Some(addr)) => eprintln!("[gateway] 已启动: http://{addr}"),
                        Ok(None) => {}
                        Err(error) => eprintln!("[gateway] 启动失败: {error}"),
                    }
                });
                // 按配置启动 Trae 网关独立监听（默认关闭；默认 127.0.0.1:7864）。
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    let runtime = handle.state::<trae_gateway::TraeGatewayRuntime>();
                    match runtime.apply().await {
                        Ok(Some(addr)) => eprintln!("[trae-gateway] 已启动: http://{addr}"),
                        Ok(None) => {}
                        Err(error) => eprintln!("[trae-gateway] 启动失败: {error}"),
                    }
                });
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_status,
            commands::get_webui_info,
            commands::get_webui_auth,
            commands::set_webui_auth,
            commands::get_accounts,
            commands::get_codebuddy_cli_status,
            commands::install_codebuddy_cli_helper,
            commands::switch_codebuddy_cli_account,
            commands::get_codebuddy_cn_ide_status,
            commands::switch_codebuddy_cn_ide_account,
            commands::detect_codebuddy_cn_ide_account,
            commands::delete_account,
            commands::oauth_start,
            commands::oauth_status,
            commands::import_local,
            commands::export_accounts,
            commands::export_accounts_to_path,
            commands::preview_import_accounts,
            commands::import_accounts,
            commands::switch_account,
            commands::switch_progress,
            commands::list_sessions,
            commands::copy_sessions,
            commands::migrate_account_data,
            commands::set_account_remark,
            commands::get_switch_config,
            commands::save_switch_config,
            commands::open_permission_settings,
            commands::check_auth_permission,
            commands::reveal_app_in_finder,
            commands::open_accounts_dir,
            commands::get_checkin_status,
            commands::get_credit_expiry,
            commands::get_credit_statistics,
            commands::get_token_statistics,
            commands::checkin,
            commands::checkin_all,
            commands::get_auto_checkin_config,
            commands::save_auto_checkin_config,
            commands::get_checkin_logs,
            commands::get_travel_status,
            commands::get_auto_travel_config,
            commands::save_auto_travel_config,
            commands::get_schedule_config,
            commands::save_schedule_config,
            commands::run_schedule_task,
            commands::refresh_account_token,
            commands::get_auto_rotate_config,
            commands::save_auto_rotate_config,
            commands::rotate_status,
            commands::run_rotate,
            commands::get_rotate_logs,
            commands::get_github_config,
            commands::save_github_config,
            commands::check_update,
            commands::relaunch_app,
            commands::get_launch_at_login_enabled,
            commands::set_launch_at_login_enabled,
            commands::get_gateway_config,
            commands::save_gateway_config,
            commands::gateway_status,
            commands::open_workbuddy_data_dir,
            commands::list_api_keys,
            commands::create_api_key,
            commands::revoke_api_key,
            commands::delete_api_key,
            commands::get_gateway_models,
            commands::refresh_gateway_models,
            commands::get_account_strategy,
            commands::save_account_strategy,
            commands::get_gateway_logs,
            commands::clear_gateway_logs,
            // ---- Trae 模块（与 server 的 /api/trae/* 路由一一对应）----
            commands::get_trae_env,
            commands::get_trae_variants,
            commands::get_trae_capabilities,
            commands::get_trae_accounts,
            commands::get_trae_checkin_status,
            commands::get_trae_credits,
            commands::get_trae_token_statistics,
            commands::get_trae_logs,
            commands::get_trae_profiles,
            commands::get_trae_settings,
            commands::save_trae_settings,
            commands::trae_add_account,
            commands::trae_update_account,
            commands::trae_set_account_remark,
            commands::trae_delete_account,
            commands::trae_import_local_account,
            commands::trae_oauth_start,
            commands::trae_oauth_status,
            commands::trae_oauth_cancel,
            commands::trae_export_accounts,
            commands::trae_export_accounts_to_path,
            commands::trae_preview_import_accounts,
            commands::trae_import_accounts,
            commands::trae_group_op,
            commands::trae_checkin,
            commands::trae_refresh_credits,
            commands::trae_refresh_jwt,
            commands::trae_clear_cooldown,
            commands::trae_switch_account,
            commands::trae_merge_legacy_regions,
            commands::trae_save_login,
            commands::trae_backup_profile,
            commands::trae_restore_profile,
            commands::trae_delete_profile,
            commands::trae_reset_device,
            // Trae API 网关（管理面）——与上面 WorkBuddy 网关的一组命令平行。
            commands::get_trae_gateway_config,
            commands::save_trae_gateway_config,
            commands::trae_gateway_status,
            commands::get_trae_gateway_models,
            commands::get_trae_client_models,
            // 多 Key 管理（含归属产品线）+ 打开数据目录（替代旧的单 Key regenerate）。
            commands::list_trae_api_keys,
            commands::create_trae_api_key,
            commands::revoke_trae_api_key,
            commands::delete_trae_api_key,
            commands::open_trae_data_dir,
            commands::trae_launch_client,
            commands::get_trae_gateway_logs,
            commands::clear_trae_gateway_logs,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|_app_handle, event| {
        #[cfg(desktop)]
        tray::on_run_event(event);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 护栏：六类定时任务**必须**全部登记在桌面端后台任务表里。
    ///
    /// 回归背景：排程循环此前只存在于 `buddy-switch-server` 二进制，桌面端只有四个写死
    /// 周期的循环，于是设置页「定时任务排程」的全部控件在桌面端空转——最典型的是
    /// 「活跃地图」从未执行，用户在官网对照连登热力图发现始终没点亮。
    ///
    /// 可证伪性：从 [`background_tasks`] 移除任一 `Scheduled`（或整段六类循环）会让本用例变红。
    #[test]
    fn background_tasks_cover_all_scheduled_tasks() {
        let tasks = background_tasks();
        for task in modules::schedule::ScheduleTask::all() {
            assert!(
                tasks.contains(&BackgroundTask::Scheduled(task)),
                "桌面端后台任务表缺少定时任务「{}」——它在此进程里永远不会执行",
                task.as_str()
            );
        }
        assert!(
            tasks.contains(&BackgroundTask::StartupMaintenance),
            "注册表缺少启动补跑任务"
        );
        assert!(
            tasks.contains(&BackgroundTask::AutoRotate),
            "注册表缺少自动轮换任务"
        );
        assert!(
            tasks.contains(&BackgroundTask::CreditsRefresh),
            "注册表缺少账号池余额刷新 —— 缺了它池里的 credits 恒为 0，\
             「四因子加权」会静默退化为两因子（服务端早已有该任务，桌面端此前漏了）"
        );
        assert!(
            tasks.contains(&BackgroundTask::PoolPersist),
            "注册表缺少账号池落盘 —— 缺了它冷却 / 熔断 / 成功率 EMA 只在内存里，重启即清零"
        );
        assert!(
            tasks.contains(&BackgroundTask::TraeCreditsRefresh),
            "注册表缺少 Trae 积分刷新 —— 缺了它纯用 API（不开界面）时积分读数永远陈旧：\
             耗尽积分的账号仍被选中、尚有余额的账号被误判为零积分"
        );
    }
}
