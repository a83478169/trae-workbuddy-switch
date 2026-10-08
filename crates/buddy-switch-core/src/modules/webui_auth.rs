//! WebUI 访问密码：PBKDF2-HMAC-SHA256 哈希 + 常量时间校验 + 配置文件读写。
//!
//! 背景：用户会把 webui 通过**反向代理**暴露到公网，因此需要密码认证
//! （HTTP Basic Auth，中间件在 `buddy-switch-server::api`）。本模块只管
//! **密码怎么存、怎么验**，以及配置文件的读写 —— 与 `update::load_github_config`
//! 同构（`store_dir()` + `atomic_write`）。
//!
//! ## 为什么用 PBKDF2 而不是裸 sha256
//!
//! 配置落在 `~/.buddy-switch/webui_auth.json`。若只存 `sha256(密码)`，文件一旦泄露，
//! 弱密码可被**秒级**字典破解。PBKDF2 迭代 10 万次把每次尝试的成本抬到几十毫秒，
//! 显著抬高离线破解门槛。选 PBKDF2 而不是 argon2/bcrypt 是因为 `pbkdf2` 已在 core
//! 的依赖树里（经 `p256` 引入），**零新增依赖**。
//!
//! ## 「没设密码」与「设了密码」是两个明确状态
//!
//! [`load`] 返回 `None` = **未启用认证**（认证中间件据此放行，这也是本地使用的默认）；
//! 返回 `Some(cfg)` = 已启用。关闭认证走 [`clear`]，**不是**保存空密码。

use std::path::PathBuf;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::modules::config::{atomic_write, store_dir};

/// PBKDF2 迭代次数。
///
/// 10 万次：release 下 ≈ 几十毫秒 —— 对**人工输入**（浏览器弹窗一次）无感，
/// 但对离线字典破解是 10 万倍成本。写进配置文件，便于将来调整而不破坏旧配置。
const PBKDF2_ITERATIONS: u32 = 100_000;

/// salt 长度（字节）。
const SALT_LEN: usize = 16;
/// 派生密钥长度（字节）。
const KEY_LEN: usize = 32;
/// 迭代次数下限。低于此值视为配置被篡改（例如把 10 万改成 1 来削弱哈希），一律拒绝。
const MIN_ITERATIONS: u32 = 10_000;
/// 密码长度上限（UTF-8 字节）。避免超长输入进入 PBKDF2。
const MAX_PASSWORD_LEN: usize = 256;

/// WebUI 访问密码配置（**只存盐与派生密钥，绝不存明文**）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebuiAuthConfig {
    /// base64 的随机盐。
    pub salt: String,
    /// base64 的 PBKDF2 派生密钥。
    pub hash: String,
    /// 迭代次数。
    pub iterations: u32,
}

/// 配置文件路径：`~/.buddy-switch/webui_auth.json`。
pub fn config_file() -> PathBuf {
    store_dir().join("webui_auth.json")
}

/// 用给定密码生成一份配置（每次调用都取新的随机盐）。
pub fn hash_password(password: &str) -> WebuiAuthConfig {
    let salt: [u8; SALT_LEN] = *uuid::Uuid::new_v4().as_bytes();
    let mut key = [0u8; KEY_LEN];
    pbkdf2::pbkdf2_hmac::<Sha256>(password.as_bytes(), &salt, PBKDF2_ITERATIONS, &mut key);
    WebuiAuthConfig {
        salt: B64.encode(salt),
        hash: B64.encode(key),
        iterations: PBKDF2_ITERATIONS,
    }
}

/// 校验密码。
///
/// 任何解析失败（base64 解不开、长度不对、盐为空）都返回 `false` —— 配置损坏时
/// **拒绝**而不是放行（见 [`load`] 对损坏文件的处理）。
pub fn verify_password(cfg: &WebuiAuthConfig, password: &str) -> bool {
    let Ok(salt) = B64.decode(&cfg.salt) else {
        return false;
    };
    let Ok(expected) = B64.decode(&cfg.hash) else {
        return false;
    };
    if salt.is_empty() || expected.len() != KEY_LEN || cfg.iterations < MIN_ITERATIONS {
        return false;
    }
    let mut actual = [0u8; KEY_LEN];
    pbkdf2::pbkdf2_hmac::<Sha256>(password.as_bytes(), &salt, cfg.iterations, &mut actual);
    constant_time_eq(&actual, &expected)
}

/// 常量时间比较。
///
/// 长度不等直接 `false` 不构成泄漏（长度是公开的）。手写而不引入 `subtle`：
/// 与 `buddy-switch-gateway::apikey` 的同名函数保持一致；`|=` 累加不含
/// data-dependent 分支，编译器不会提前退出，`black_box` 再兜一层。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    std::hint::black_box(diff) == 0
}

/// 从 `Authorization: Basic <base64>` 头里取出**密码**。
///
/// 用户名被忽略：本应用只有一个用户，密码才是秘密。返回 `None` 表示这不是一个
/// 合法的 Basic 凭据（scheme 不符 / base64 解不开 / 非 UTF-8 / 没有冒号分隔符）。
pub fn password_from_basic_header(header: &str) -> Option<String> {
    let (scheme, encoded) = header.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = B64.decode(encoded.trim()).ok()?;
    let text = String::from_utf8(decoded).ok()?;
    // 只取第一个冒号之后的部分 —— 密码里可以含冒号。
    let (_user, password) = text.split_once(':')?;
    Some(password.to_string())
}

/// 读取配置。`None` = **未启用认证**（文件不存在）。
///
/// ⚠️ 文件存在但**损坏**时，返回一个**永远验证不过**的配置（空盐/空哈希/
/// 迭代 0），而不是 `None` —— 否则一次文件损坏就会让 webui 静默裸奔。
/// 用户可以经桌面端（走 Tauri IPC，不经过 HTTP 认证）重设密码恢复。
pub fn load() -> Option<WebuiAuthConfig> {
    let text = std::fs::read_to_string(config_file()).ok()?;
    match serde_json::from_str::<WebuiAuthConfig>(&text) {
        Ok(cfg) => Some(cfg),
        Err(error) => {
            eprintln!("[webui-auth] 配置损坏，已按「永久拒绝」处理（请在桌面端重设密码）：{error}");
            Some(WebuiAuthConfig {
                salt: String::new(),
                hash: String::new(),
                iterations: 0,
            })
        }
    }
}

/// 写配置（原子写）。
pub fn save(cfg: &WebuiAuthConfig) -> std::io::Result<()> {
    std::fs::create_dir_all(store_dir())?;
    let text = serde_json::to_string_pretty(cfg).unwrap_or_default();
    atomic_write(&config_file(), &text)
}

/// 设置密码：空 / 纯空白 / 超长返回错误（关闭认证请用 [`clear`]，不要存空密码）。
pub fn set_password(password: &str) -> std::io::Result<()> {
    if password.trim().is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "密码不能为空",
        ));
    }
    if password.len() > MAX_PASSWORD_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "密码过长",
        ));
    }
    save(&hash_password(password))
}

/// 清除配置（关闭认证）。文件不存在也算成功。
pub fn clear() -> std::io::Result<()> {
    match std::fs::remove_file(config_file()) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 哈希 → 校验：正确密码通过，错误密码不通过。
    #[test]
    fn hash_then_verify_roundtrip() {
        let cfg = hash_password("correct horse battery staple");
        assert!(verify_password(&cfg, "correct horse battery staple"));
        assert!(!verify_password(&cfg, "wrong"));
        assert!(!verify_password(&cfg, ""));
        // 大小写敏感。
        assert!(!verify_password(&cfg, "Correct horse battery staple"));
    }

    /// 同一个密码两次哈希的盐不同 ⇒ 配置不同（盐是随机的），但都能验证通过。
    #[test]
    fn salt_is_random_per_hash() {
        let a = hash_password("same");
        let b = hash_password("same");
        assert_ne!(a.salt, b.salt, "两次哈希的盐必须不同");
        assert_ne!(a.hash, b.hash, "派生密钥也应不同");
        assert!(verify_password(&a, "same") && verify_password(&b, "same"));
    }

    /// 损坏 / 伪造的配置一律验证失败（fail-closed）。
    #[test]
    fn broken_config_never_verifies() {
        for cfg in [
            WebuiAuthConfig { salt: String::new(), hash: String::new(), iterations: 0 },
            WebuiAuthConfig { salt: "!!!not-base64!!!".into(), hash: "x".into(), iterations: PBKDF2_ITERATIONS },
            WebuiAuthConfig { salt: B64.encode([0u8; SALT_LEN]), hash: B64.encode([0u8; 8]), iterations: PBKDF2_ITERATIONS },
        ] {
            assert!(!verify_password(&cfg, "anything"), "损坏配置必须拒绝：{cfg:?}");
            assert!(!verify_password(&cfg, ""), "损坏配置必须拒绝空密码：{cfg:?}");
        }
    }

    /// 空 / 纯空白密码不得被设置（关闭认证要用 clear）。
    #[test]
    fn set_password_rejects_blank() {
        assert!(set_password("").is_err());
        assert!(set_password("   ").is_err());
        assert!(set_password("\t\n").is_err());
    }

    /// 解析 `Authorization: Basic` 头：只取密码、忽略用户名、容忍大小写与密码里的冒号。
    #[test]
    fn basic_header_parsing() {
        let enc = |s: &str| B64.encode(s.as_bytes());
        assert_eq!(
            password_from_basic_header(&format!("Basic {}", enc("admin:p@ss"))),
            Some("p@ss".to_string())
        );
        // scheme 大小写不敏感。
        assert_eq!(
            password_from_basic_header(&format!("basic {}", enc("u:p"))),
            Some("p".to_string())
        );
        // 用户名随便填、密码里的冒号保留。
        assert_eq!(
            password_from_basic_header(&format!("Basic {}", enc("whatever:a:b:c"))),
            Some("a:b:c".to_string())
        );
        // 非法输入一律 None。
        assert_eq!(password_from_basic_header("Bearer abc"), None);
        assert_eq!(password_from_basic_header("Basic !!!not-base64!!!"), None);
        assert_eq!(password_from_basic_header(&format!("Basic {}", enc("no-colon"))), None);
        assert_eq!(password_from_basic_header("Basic"), None);
    }
}
