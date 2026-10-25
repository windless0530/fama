//! 从默认 Chromium 系浏览器导入登录 cookies（design.md §2.5）。
//! 两条入口共用 [`import`]：启动自动导入 [`import_auto`]（每进程一次，
//! App mount 后前端触发）与手动导入（🔑 ImportDialog，用户确认后可反复重试）。
//!
//! 范围（有意收窄）：
//! - 只识别默认浏览器是否为 Chromium 系（Chrome / Edge / Brave / Vivaldi / Opera /
//!   Chromium）；Safari / Firefox / 未知直接报不支持，且不触发 Keychain 弹窗；
//! - 读取该浏览器的 `"<Browser> Safe Storage"` 钥匙串条目（macOS 会弹 ACL 授权框，
//!   为第二道用户授权），PBKDF2 派生 cookie 解密 key；
//! - 以临时副本只读查询浏览器 Cookies 数据库，只筛选已配置站点 host 匹配的条目；
//! - 扫描全部 profile（`Local State` 可读时取 `profile.info` 全集、last_used
//!   优先；不可读时探测已知命名 `Default` / `Profile 1..9`），结果按
//!   (host, name) 去重合并；
//! - 硬前提：macOS TCC 保护浏览器数据目录，运行环境需「完全磁盘访问」授权
//!   （dev = 运行它的终端 App，安装版 = fama.app）；权限拒绝（EPERM/EACCES）
//!   报 `E_BROWSER_DATA_TCC_DENIED`，一次失败即返回，不重试；
//! - 解密后注入 WKWebsiteDataStore.defaultDataStore()（所有站点 webview 共享），
//!   并 reload 已加载的站点页面恢复登录态。
//!
//! 明确不做：读取浏览器密码库（Login Data）、Safari 密码 / cookies（TCC 与 ACL
//! 限制，第三方 WKWebView app 无法稳定读取）。
//!
//! 数据安全：Safe Storage 原始 secret 读出即 zeroize；派生 key 仅缓存在本次进程
//! 内存（重复导入不重复弹 Keychain）；Cookies 数据库以临时目录副本查询，任何
//! 退出路径都删除副本。解密后的明文 cookie 只进入本 app 自己的 WebKit 数据
//! 容器（defaultDataStore，仅本 app 可见），不做中间落盘；但带有效期的 cookie
//! 可能由 WebKit 持久化到本机容器内——这与导入后在 app 里正常登录产生的 cookie
//! 行为一致。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Manager, Wry};
use zeroize::Zeroizing;

use crate::local_storage_db::{self, LsEntry};
use crate::sites::SiteConfig;
use crate::tabs::Tabs;

// ---------- 错误码（沿用 commands.rs 的 "E_XXX" 约定，见模块 import() 返回） ----------

pub const E_NO_SITES: &str = "E_NO_SITES";
pub const E_DETECT_FAILED: &str = "E_DETECT_FAILED";
pub const E_UNSUPPORTED_BROWSER: &str = "E_UNSUPPORTED_BROWSER";
pub const E_KEYCHAIN_NOT_FOUND: &str = "E_KEYCHAIN_NOT_FOUND";
pub const E_KEYCHAIN_DENIED: &str = "E_KEYCHAIN_DENIED";
pub const E_BROWSER_DATA_MISSING: &str = "E_BROWSER_DATA_MISSING";
pub const E_BROWSER_DATA_TCC_DENIED: &str = "E_BROWSER_DATA_TCC_DENIED";
pub const E_COOKIE_DB_LOCKED: &str = "E_COOKIE_DB_LOCKED";
pub const E_DECRYPT_FAILED: &str = "E_DECRYPT_FAILED";
pub const E_INJECT_FAILED: &str = "E_INJECT_FAILED";
/// 仅自动导入路径：本进程已尝试过一次（前端收到后应静默忽略，不弹窗）
pub const E_AUTO_ALREADY_RUN: &str = "E_AUTO_ALREADY_RUN";

/// 带 "E_XXX" 错误码的导入失败（commands 层拼成 "code: message" 给前端）
#[derive(Debug)]
pub struct ImportError {
    pub code: &'static str,
    pub message: String,
}

impl ImportError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

// ---------- 诊断日志 ----------
//
// 每个阶段一条 `[import]` 结构化日志（对齐 [tabs] / [preload] 的既有风格），
// 用于真机定位「导入不生效」卡在哪个环节。FAMA_DEBUG_IMPORT=1 时额外打印
// 每条 cookie 的 host/name/属性（永不打印 value）。

fn debug_import() -> bool {
    std::env::var("FAMA_DEBUG_IMPORT").as_deref() == Ok("1")
}

macro_rules! ilog {
    ($($arg:tt)*) => { eprintln!("[import] {}", format_args!($($arg)*)) };
}

// ---------- 浏览器映射 ----------

/// 一个 Chromium 系浏览器的全部平台差异信息
struct ChromiumBrowser {
    /// 展示名
    name: &'static str,
    /// bundle id（默认浏览器识别结果匹配任一即认为是该浏览器）
    bundle_ids: &'static [&'static str],
    /// Keychain generic password 的 service（条目名）
    safe_storage_service: &'static str,
    /// Keychain generic password 的 account
    safe_storage_account: &'static str,
    /// `~/Library/Application Support` 下的用户数据目录
    data_dir: &'static str,
}

const BROWSERS: &[ChromiumBrowser] = &[
    ChromiumBrowser {
        name: "Chrome",
        bundle_ids: &["com.google.Chrome"],
        safe_storage_service: "Chrome Safe Storage",
        safe_storage_account: "Chrome",
        data_dir: "Google/Chrome",
    },
    ChromiumBrowser {
        name: "Edge",
        bundle_ids: &["com.microsoft.edgemac"],
        safe_storage_service: "Microsoft Edge Safe Storage",
        safe_storage_account: "Microsoft Edge",
        data_dir: "Microsoft Edge",
    },
    ChromiumBrowser {
        name: "Brave",
        bundle_ids: &["com.brave.Browser"],
        safe_storage_service: "Brave Safe Storage",
        safe_storage_account: "Brave",
        data_dir: "BraveSoftware/Brave-Browser",
    },
    ChromiumBrowser {
        name: "Vivaldi",
        bundle_ids: &["com.vivaldi.Vivaldi"],
        safe_storage_service: "Vivaldi Safe Storage",
        safe_storage_account: "Vivaldi",
        data_dir: "Vivaldi",
    },
    ChromiumBrowser {
        name: "Opera",
        bundle_ids: &["com.operasoftware.Opera"],
        safe_storage_service: "Opera Safe Storage",
        safe_storage_account: "Opera",
        data_dir: "com.operasoftware.Opera",
    },
    ChromiumBrowser {
        name: "Chromium",
        bundle_ids: &["org.chromium.Chromium"],
        safe_storage_service: "Chromium Safe Storage",
        safe_storage_account: "Chromium",
        data_dir: "Chromium",
    },
];

fn browser_for_bundle_id(bundle_id: &str) -> Option<&'static ChromiumBrowser> {
    BROWSERS.iter().find(|b| b.bundle_ids.contains(&bundle_id))
}

// ---------- 默认浏览器识别 ----------

/// 问 LaunchServices「https:// 链接用哪个 app 打开」，返回其 bundle id。
/// 在主线程执行（AppKit 约定），同步等待结果。
fn detect_default_browser_bundle_id(app: &AppHandle<Wry>) -> Result<String, ImportError> {
    let (tx, rx) = mpsc::channel();
    app.run_on_main_thread(move || {
        let res: Result<String, ImportError> = (|| {
            use objc2_app_kit::NSWorkspace;
            use objc2_foundation::{NSBundle, NSString, NSURL};

            let probe = NSString::from_str("https://example.com/");
            let url = NSURL::URLWithString(&probe)
                .ok_or_else(|| ImportError::new(E_DETECT_FAILED, "构造探测 URL 失败"))?;
            let app_url = NSWorkspace::sharedWorkspace()
                .URLForApplicationToOpenURL(&url)
                .ok_or_else(|| {
                    ImportError::new(E_DETECT_FAILED, "系统未设置默认浏览器")
                })?;
            let bundle = NSBundle::bundleWithURL(&app_url)
                .ok_or_else(|| ImportError::new(E_DETECT_FAILED, "读取浏览器 app bundle 失败"))?;
            let id = bundle.bundleIdentifier().ok_or_else(|| {
                ImportError::new(E_DETECT_FAILED, "浏览器 app 无 bundle id")
            })?;
            Ok(id.to_string())
        })();
        let _ = tx.send(res);
    })
    .map_err(|e| ImportError::new(E_DETECT_FAILED, format!("派发主线程失败: {e}")))?;
    rx.recv_timeout(Duration::from_secs(5))
        .map_err(|_| ImportError::new(E_DETECT_FAILED, "默认浏览器识别超时"))?
}

// ---------- Keychain：Safe Storage 读取与派生 key ----------

/// errSecItemNotFound：条目不存在（浏览器从未在这台机器存过凭据）
const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;
/// errSecAuthFailed / errSecInteractionNotAllowed：用户在 ACL 弹窗点了拒绝
const ERR_SEC_AUTH_FAILED: i32 = -25293;
const ERR_SEC_INTERACTION_NOT_ALLOWED: i32 = -25308;

/// 派生 key 的进程内缓存（browser.name → key）。
/// 只在内存：重复导入同一浏览器不再触发 Keychain ACL 弹窗。
type DerivedKeyCache = HashMap<&'static str, Zeroizing<[u8; 16]>>;
static DERIVED_KEYS: Mutex<Option<DerivedKeyCache>> = Mutex::new(None);

/// PBKDF2-HMAC-SHA1。独立出来是为了能用 RFC 6070 已知向量做单测。
pub(crate) fn pbkdf2_sha1(password: &[u8], salt: &[u8], rounds: u32, out: &mut [u8]) {
    use hmac::Hmac;
    use sha1::Sha1;
    // out 足够容纳 HMAC-SHA1 的 20 字节输出时不会失败；不足时截断输出（无效长度返回 Err）
    let _ = pbkdf2::pbkdf2::<Hmac<Sha1>>(password, salt, rounds, out);
}

/// Chromium macOS 约定：key = PBKDF2-HMAC-SHA1(safe_storage, "saltysalt", 1003, 16B)
fn derive_cookie_key(secret: &[u8]) -> Zeroizing<[u8; 16]> {
    let mut key = Zeroizing::new([0u8; 16]);
    pbkdf2_sha1(secret, b"saltysalt", 1003, key.as_mut());
    key
}

/// 读取（或复用缓存的）派生 cookie key。原始 secret 只在本函数栈上存在，返回前 zeroize。
fn derived_key_for(browser: &ChromiumBrowser) -> Result<Zeroizing<[u8; 16]>, ImportError> {
    {
        let mut guard = DERIVED_KEYS.lock().unwrap();
        if let Some(k) = guard.get_or_insert_with(DerivedKeyCache::new).get(browser.name) {
            ilog!("keychain: reusing cached derived key (no ACL prompt)");
            return Ok(k.clone());
        }
    }
    ilog!(
        "keychain: reading \"{}\" (macOS 会弹 ACL 授权框，等待用户应答…)",
        browser.safe_storage_service
    );
    let secret: Zeroizing<Vec<u8>> =
        match security_framework::passwords::get_generic_password(
            browser.safe_storage_service,
            browser.safe_storage_account,
        ) {
            Ok(v) => Zeroizing::new(v),
            Err(e) => {
                return Err(match e.code() {
                    ERR_SEC_ITEM_NOT_FOUND => ImportError::new(
                        E_KEYCHAIN_NOT_FOUND,
                        format!(
                            "钥匙串中未找到 \"{}\" 条目（浏览器可能从未保存过密码）",
                            browser.safe_storage_service
                        ),
                    ),
                    ERR_SEC_AUTH_FAILED | ERR_SEC_INTERACTION_NOT_ALLOWED => {
                        ImportError::new(E_KEYCHAIN_DENIED, "钥匙串授权被拒绝")
                    }
                    code => ImportError::new(
                        E_KEYCHAIN_DENIED,
                        format!("读取钥匙串失败（code {code}）"),
                    ),
                });
            }
        };
    let key = derive_cookie_key(&secret);
    DERIVED_KEYS
        .lock()
        .unwrap()
        .get_or_insert_with(DerivedKeyCache::new)
        .insert(browser.name, key.clone());
    Ok(key)
}

// ---------- v10 解密 ----------

/// macOS Chromium `encrypted_value` 的失败分类
#[derive(Debug, PartialEq)]
pub(crate) enum DecryptError {
    /// 前缀不是 "v10"（未来加密方案变更，如 app-bound 的 v20）
    NotV10,
    /// 密文长度不是 16 的倍数 / 为空
    BadLength,
    /// AES-CBC 解密后 PKCS7 去填充失败（key 不对或数据损坏）
    BadPadding,
}

/// 解密 macOS Chromium cookie 的 v10 格式：
/// `"v10" + AES-128-CBC(key, IV=16 个空格, PKCS7(plaintext))`
pub(crate) fn decrypt_v10(key: &[u8; 16], encrypted: &[u8]) -> Result<Vec<u8>, DecryptError> {
    use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
    type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

    let body = encrypted.strip_prefix(b"v10").ok_or(DecryptError::NotV10)?;
    if body.is_empty() || body.len() % 16 != 0 {
        return Err(DecryptError::BadLength);
    }
    let iv = [b' '; 16];
    let dec = Aes128CbcDec::new_from_slices(key, &iv).map_err(|_| DecryptError::BadLength)?;
    dec.decrypt_padded_vec_mut::<Pkcs7>(body)
        .map_err(|_| DecryptError::BadPadding)
}

/// Edge 156+（实测 156.0.4314.42，macOS 版 app-bound 等价方案）：v10 解密后
/// 的明文不是 cookie 值，而是 `SHA-256(host_key)[32B] + value`——与 Windows
/// v20 的 domain hash 前缀同款（不同 host_key 的前缀不同，可精确校验）。
/// hash 匹配 → 返回剥掉前缀后的值；不匹配（旧版无前缀格式）→ None，
/// 调用方按「明文即值」的旧格式处理。
fn strip_domain_hash_prefix<'a>(plain: &'a [u8], host_key: &str) -> Option<&'a [u8]> {
    use sha2::{Digest, Sha256};
    if plain.len() < 32 {
        return None;
    }
    let hash = Sha256::digest(host_key.as_bytes());
    plain.strip_prefix(hash.as_slice())
}

// ---------- 时间与 host 匹配 ----------

/// Chromium cookies 表时间戳（1601-01-01 起的微秒数）→ Unix epoch 秒（NSDate 用）
pub(crate) fn chromium_utc_to_unix_secs(utc: i64) -> f64 {
    const CHROMIUM_EPOCH_OFFSET_SECS: f64 = 11_644_473_600.0;
    utc as f64 / 1_000_000.0 - CHROMIUM_EPOCH_OFFSET_SECS
}

/// cookie 的 host_key 是否会发送到站点 host（模拟浏览器的 cookie 域匹配规则）：
/// - host_key 带前导 '.'（域 cookie）：站点 host 等于该域或为其子域 → 匹配；
/// - 否则（host-only cookie）：仅精确相等匹配。
pub(crate) fn host_matches(cookie_host: &str, site_host: &str) -> bool {
    let site = site_host.trim_matches('.').to_ascii_lowercase();
    if site.is_empty() {
        return false;
    }
    if let Some(dom) = cookie_host.strip_prefix('.') {
        let dom = dom.to_ascii_lowercase();
        site == dom || site.ends_with(&format!(".{dom}"))
    } else {
        cookie_host.trim_end_matches('.').eq_ignore_ascii_case(&site)
    }
}

// ---------- Cookies 数据库读取 ----------

/// skip 计数（按原因细分，「不再有不可解释的 skipped_other」）。
/// expired / utf8 / decrypt / read 四类是真正跳过不导入；
/// `expires_unknown` 特殊：expires_utc 列读不出有效数值（NULL / 类型不符）
/// 的条目不丢弃、按会话 cookie 保留，单独计数供日志定位 schema 异常。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SkipStats {
    /// has_expires 且换算后的 expires_utc 已过当前时间
    pub expired: usize,
    /// 解密后的明文不是合法 UTF-8
    pub utf8: usize,
    /// v10 解密失败（非 v10 / 长度 / padding）
    pub decrypt: usize,
    /// host_key 等关键列读取失败
    pub read: usize,
    /// expires_utc 读不出有效数值但保留（会话 cookie）的条数
    pub expires_unknown: usize,
}

impl SkipStats {
    /// 对应 ImportReport.skipped_other 的既有语义（未导入且非解密失败）
    fn other(&self) -> usize {
        self.expired + self.utf8 + self.read
    }
}

/// SQLite 值宽松读 i64。Chromium 的 expires_utc 等时间戳列历史上是 INTEGER，
/// 但新版本 / 分支（如 Edge 156 实测出现整库误判过期）可能读出 NULL、REAL
/// 或 TEXT；用 `unwrap_or(0)` 静默成 0 会被过期判断换算成 1601 年附近而误杀。
/// INTEGER 直取；REAL / TEXT 尝试按整数数值解释；NULL / 其余 → None。
fn value_ref_as_i64(v: rusqlite::types::ValueRef<'_>) -> Option<i64> {
    use rusqlite::types::ValueRef;
    match v {
        ValueRef::Integer(i) => Some(i),
        ValueRef::Real(f) if f.is_finite() && f.fract() == 0.0 && f.abs() < 9.2e18 => {
            Some(f as i64)
        }
        ValueRef::Text(t) => std::str::from_utf8(t)
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok()),
        ValueRef::Null | ValueRef::Real(_) | ValueRef::Blob(_) => None,
    }
}

/// 解密前从数据库读到的一条 cookie
#[derive(Debug, Clone)]
pub(crate) struct RawCookie {
    pub host_key: String,
    pub name: String,
    pub value: String,
    pub path: String,
    /// 0 / has_expires=false 表示会话 cookie
    pub expires_utc: i64,
    pub has_expires: bool,
    pub is_secure: bool,
    pub is_httponly: bool,
    /// -1 未指定 / 0 None / 1 Lax / 2 Strict
    pub samesite: i64,
}

/// 临时目录守卫：任何退出路径删除整个副本目录
struct TempDirGuard(PathBuf);
impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ---------- TCC / 权限拒绝识别 ----------
//
// macOS 把 Chromium 系浏览器的用户数据目录纳入 TCC 隐私保护：fama 进程
// （dev 下从终端 App 派生）读取 `Local State` / `Cookies` 文件内容时得到
// EPERM / EACCES（Rust 归类为 `ErrorKind::PermissionDenied`），但 stat /
// is_file() 元数据探测不受影响。据此在错误分类里把权限拒绝单独拎出，
// 给出「完全磁盘访问」的可操作指引，而不是误报数据缺失。

/// `E_BROWSER_DATA_TCC_DENIED` 的完整用户提示。
/// `what`：被拦截的读取目标（如 "Cookies 数据库"）。
fn tcc_denied(browser_name: &str, what: &str) -> ImportError {
    ImportError::new(
        E_BROWSER_DATA_TCC_DENIED,
        format!(
            "{browser_name} 的浏览器数据（{what}）受 macOS 隐私保护（TCC）拦截，fama 无法读取。\
             请在「系统设置 → 隐私与安全性 → 完全磁盘访问」中授权：开发模式\
             （npm run tauri dev）给运行它的终端 App（如 iTerm2 / Terminal）授权；\
             安装版给 fama.app 授权。授权后完全退出并重启该 App 再试"
        ),
    )
}

/// 浏览器数据读取的 io 错误分类：权限拒绝（EPERM / EACCES）→ TCC 错误
/// （`local_state_denied` 时在提示里合并 Local State 同被拦截的信号）；
/// 其余错误归为 DB 读取问题并保留 `action` 上下文。
fn browser_read_err(
    e: &std::io::Error,
    browser_name: &str,
    action: &str,
    local_state_denied: bool,
) -> ImportError {
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        let what = if local_state_denied {
            "Cookies 数据库与 Local State"
        } else {
            "Cookies 数据库"
        };
        tcc_denied(browser_name, what)
    } else {
        ImportError::new(E_COOKIE_DB_LOCKED, format!("{action}: {e}"))
    }
}

// ---------- 多 profile 枚举 ----------

/// profile 扫描计划
struct ProfileScan {
    /// 有序 profile 目录名（last_used 优先，其余按名称字典序）
    profiles: Vec<String>,
    /// `Local State` 读取被权限拒绝（TCC 强信号；不影响继续探测）
    local_state_denied: bool,
}

/// 从 `Local State` JSON 解析有序 profile 名。
/// 返回 `profile.info` 的 key 集合（保证含 "Default"），`last_used` 优先、
/// 其余按字典序。None = JSON 解析失败或没有 `profile.info`（调用方回退探测）。
fn profiles_from_local_state(txt: &str) -> Option<Vec<String>> {
    let v = serde_json::from_str::<serde_json::Value>(txt).ok()?;
    let info = v.pointer("/profile/info")?.as_object()?;
    let mut names: Vec<String> = info.keys().cloned().collect();
    if names.is_empty() {
        return None;
    }
    if !names.iter().any(|n| n == "Default") {
        names.push("Default".to_string());
    }
    names.sort();
    names.dedup();
    let last_used = v.pointer("/profile/last_used").and_then(|x| x.as_str());
    let mut ordered: Vec<String> = Vec::with_capacity(names.len());
    if let Some(lu) = last_used.filter(|lu| names.iter().any(|n| n == lu)) {
        ordered.push(lu.to_string());
    }
    ordered.extend(names.into_iter().filter(|n| Some(n.as_str()) != last_used));
    Some(ordered)
}

/// 回退探测（`Local State` 不可读 / 无 profile.info 时）：已知命名
/// `Default`、`Profile 1`..`Profile 9` 中，`is_file()` 元数据探测（TCC 下
/// 仍可用）能命中 Cookies 候选的才进入扫描。
fn probe_known_profiles(base: &Path) -> Vec<String> {
    let mut names = vec!["Default".to_string()];
    names.extend((1..=9).map(|i| format!("Profile {i}")));
    names
        .into_iter()
        .filter(|n| find_cookie_db(base, n).is_some())
        .collect()
}

/// 枚举要扫描的 profile：
/// - `Local State` 可读：`profile.info` 的 key 集合（含 "Default"），
///   `last_used` 优先；
/// - 不可读（EPERM 等权限拒绝记为 TCC 强信号）或解析失败：回退探测已知命名。
fn list_profiles(base: &Path) -> ProfileScan {
    match std::fs::read_to_string(base.join("Local State")) {
        Ok(txt) => match profiles_from_local_state(&txt) {
            Some(profiles) => ProfileScan {
                profiles,
                local_state_denied: false,
            },
            None => ProfileScan {
                profiles: probe_known_profiles(base),
                local_state_denied: false,
            },
        },
        Err(e) => ProfileScan {
            profiles: probe_known_profiles(base),
            local_state_denied: e.kind() == std::io::ErrorKind::PermissionDenied,
        },
    }
}

/// Cookies 数据库的有序候选路径。Chromium 各分支布局不一致：
/// - Chrome / Edge / Brave / Vivaldi：`{profile}/Cookies`（新版本迁到
///   `{profile}/Network/Cookies`）；
/// - Opera 稳定版：常用数据根目录下的 `Cookies` / `Network/Cookies`，
///   不一定有 profile 子目录。
///
/// profile 目录优先于数据根，`Cookies` 优先于 `Network/Cookies`，
/// 按此顺序探测第一个存在的文件。
fn cookie_db_candidates(base: &Path, profile: &str) -> Vec<PathBuf> {
    let profile_dir = base.join(profile);
    vec![
        profile_dir.join("Cookies"),
        profile_dir.join("Network").join("Cookies"),
        base.join("Cookies"),
        base.join("Network").join("Cookies"),
    ]
}

/// 依次探测候选路径，返回第一个存在的 Cookies 数据库
fn find_cookie_db(base: &Path, profile: &str) -> Option<PathBuf> {
    cookie_db_candidates(base, profile)
        .into_iter()
        .find(|p| p.is_file())
}

/// 复制 Cookies(+wal/shm) 到临时目录并以 SQLite 打开。
/// 用读写方式打开我们自己拥有的副本（让 SQLite 完成 WAL 恢复），随后 query_only。
/// 复制源文件被权限拒绝（TCC）→ `E_BROWSER_DATA_TCC_DENIED`，一次失败即返回。
fn open_cookie_copy(
    src: &Path,
    browser_name: &str,
    local_state_denied: bool,
) -> Result<(rusqlite::Connection, TempDirGuard), ImportError> {
    let tmp = std::env::temp_dir().join(format!(
        "fama-cookies-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    std::fs::create_dir_all(&tmp)
        .map_err(|e| ImportError::new(E_COOKIE_DB_LOCKED, format!("创建临时目录失败: {e}")))?;
    let guard = TempDirGuard(tmp.clone());
    let dst = tmp.join("Cookies");
    for suffix in ["", "-wal", "-shm"] {
        let from = src.with_file_name(format!("Cookies{suffix}"));
        if from.exists() {
            if let Err(e) = std::fs::copy(&from, dst.with_file_name(format!("Cookies{suffix}"))) {
                return Err(browser_read_err(
                    &e,
                    browser_name,
                    "复制 Cookies 数据库失败",
                    local_state_denied,
                ));
            }
        }
    }
    let conn = rusqlite::Connection::open(&dst)
        .map_err(|e| ImportError::new(E_COOKIE_DB_LOCKED, format!("打开 Cookies 副本失败: {e}")))?;
    // 副本只用于查询；防误写
    if conn
        .pragma_update(None, "query_only", true)
        .is_err()
    {
        let _ = conn.execute_batch("PRAGMA query_only = ON;");
    }
    Ok((conn, guard))
}

/// 查询 + host 过滤 + 解密。返回 (matched, skip 细分计数, cookies)。
/// 每条被跳过的 cookie 在 FAMA_DEBUG_IMPORT=1 时打印原因与关键字段
/// （expires_utc 原始值 / 换算 unix 秒 / now / 解密错误类别 / 明文前 8 字节
/// hex），永不打印完整 value。
fn read_and_decrypt(
    conn: &rusqlite::Connection,
    key: &[u8; 16],
    site_hosts: &[String],
) -> Result<(usize, SkipStats, Vec<RawCookie>), ImportError> {
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let mut stmt = conn
        .prepare(
            "SELECT host_key, name, value, encrypted_value, path, expires_utc, \
             is_secure, is_httponly, has_expires, samesite FROM cookies",
        )
        .map_err(|e| ImportError::new(E_COOKIE_DB_LOCKED, format!("查询 cookies 表失败: {e}")))?;

    // 侦察日志（每库一次，debug 时）：schema 原文 + 库的新旧。定位新版本
    // 浏览器 schema / 列类型变化（列存在但值形态异常）与「读到已弃用旧库」
    // （条目真实过期）两类不同的整库异常。
    if debug_import() {
        if let Ok(sql) = conn.query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='cookies'",
            [],
            |r| r.get::<_, String>(0),
        ) {
            ilog!("db schema: {sql}");
        }
        if let Ok((rows, max_access)) = conn.query_row(
            "SELECT COUNT(*), IFNULL(MAX(last_access_utc), 0) FROM cookies",
            [],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        ) {
            ilog!(
                "db stats: rows={rows} max_last_access_utc={max_access} (≈ unix {})",
                chromium_utc_to_unix_secs(max_access)
            );
        }
    }

    let mut rows = stmt
        .query([])
        .map_err(|e| ImportError::new(E_COOKIE_DB_LOCKED, format!("遍历 cookies 表失败: {e}")))?;

    let mut matched = 0;
    let mut skips = SkipStats::default();
    let mut cookies = Vec::new();
    loop {
        let row = match rows.next() {
            Ok(Some(r)) => r,
            Ok(None) => break,
            Err(_) => {
                return Err(ImportError::new(
                    E_COOKIE_DB_LOCKED,
                    "遍历 cookies 表失败（数据库副本可能损坏）",
                ))
            }
        };
        let host_key: String = match row.get(0) {
            Ok(v) => v,
            Err(e) => {
                if debug_import() {
                    ilog!("skip read: col=host_key err={e}");
                }
                skips.read += 1;
                continue;
            }
        };
        if !site_hosts.iter().any(|h| host_matches(&host_key, h)) {
            continue;
        }
        matched += 1;
        let name: String = row.get(1).unwrap_or_default();
        let value: String = row.get(2).unwrap_or_default();
        let encrypted: Vec<u8> = row.get(3).unwrap_or_default();
        let path: String = row.get(4).unwrap_or_else(|_| "/".to_string());
        // expires_utc / has_expires 宽松读取：读不出有效数值（NULL / 类型
        // 不符），或 expires_utc=0 却声明 has_expires=1 的矛盾行（旧逻辑把
        // 0 换算到 1601 年附近 → 整库误杀的最大嫌疑形态），都不再判过期，
        // 改按「无有效期信息」保留为会话 cookie，并计数 + 记原始值形态供诊断。
        // expires_utc=0 且 has_expires=0 是正常会话 cookie，不计数。
        let has_expires_col: bool = row.get::<_, i64>(8).map(|v| v != 0).unwrap_or(false);
        let expires_raw: Option<i64> = row.get_ref(5).ok().and_then(value_ref_as_i64);
        let (expires_utc, has_expires) = match expires_raw {
            Some(v) if v != 0 => (v, has_expires_col),
            _ => {
                // expires_utc=0 且 has_expires=0 是正常会话 cookie，不计数不打日志
                if has_expires_col || expires_raw.is_none() {
                    skips.expires_unknown += 1;
                    if debug_import() {
                        let raw = row
                            .get_ref(5)
                            .map(|v| format!("{v:?}"))
                            .unwrap_or_else(|e| format!("读列失败: {e}"));
                        ilog!(
                            "expires unknown: host={host_key} name={name} raw={raw} \
                             has_expires_col={has_expires_col}"
                        );
                    }
                }
                (0, false)
            }
        };
        let is_secure: bool = row.get::<_, i64>(6).map(|v| v != 0).unwrap_or(false);
        let is_httponly: bool = row.get::<_, i64>(7).map(|v| v != 0).unwrap_or(false);
        let samesite: i64 = row.get(9).unwrap_or(-1);

        // 过期 cookie 不导入（换算值与当前时间都打进日志，真凶一眼可见）
        if has_expires && chromium_utc_to_unix_secs(expires_utc) <= now_unix {
            if debug_import() {
                ilog!(
                    "skip expired: host={host_key} name={name} has_expires=true \
                     expires_utc={expires_utc} unix={} now_unix={now_unix}",
                    chromium_utc_to_unix_secs(expires_utc)
                );
            }
            skips.expired += 1;
            continue;
        }
        let plain: Vec<u8> = if encrypted.is_empty() {
            value.into_bytes()
        } else {
            match decrypt_v10(key, &encrypted) {
                Ok(p) => p,
                Err(e) => {
                    if debug_import() {
                        ilog!(
                            "skip decrypt: host={host_key} name={name} enc_len={} reason={e:?}",
                            encrypted.len()
                        );
                    }
                    skips.decrypt += 1;
                    continue;
                }
            }
        };
        // Edge 156+：v10 明文带 SHA-256(host_key) 前缀，先剥掉再当值；
        // hash 不匹配（旧格式）按整段明文当值
        let body: &[u8] = strip_domain_hash_prefix(&plain, &host_key).unwrap_or(&plain);
        let value_str = match std::str::from_utf8(body) {
            Ok(s) => s.to_string(),
            Err(_) => {
                if debug_import() {
                    // 明文前 8 字节 hex 足以判断格式，永不打印完整 value
                    let head: Vec<String> =
                        plain.iter().take(8).map(|b| format!("{b:02x}")).collect();
                    ilog!(
                        "skip utf8: host={host_key} name={name} plain_len={} plain_head=[{}]",
                        plain.len(),
                        head.join(" ")
                    );
                }
                skips.utf8 += 1;
                continue;
            }
        };
        cookies.push(RawCookie {
            host_key,
            name,
            value: value_str,
            path,
            expires_utc,
            has_expires,
            is_secure,
            is_httponly,
            samesite,
        });
    }
    Ok((matched, skips, cookies))
}

// ---------- 多 profile 扫描与合并 ----------

/// 合并一条 cookie：按 (host_key, name) 去重，重复（跨 profile 常见）只保留
/// 先扫描到的（扫描顺序靠前的 profile 优先）。`seen` 为已占用键集合。
fn push_dedup(
    cookies: &mut Vec<RawCookie>,
    seen: &mut HashSet<(String, String)>,
    c: RawCookie,
) {
    if seen.insert((c.host_key.clone(), c.name.clone())) {
        cookies.push(c);
    }
}

/// 多 profile 扫描的聚合结果
#[derive(Debug)]
struct ScanOutcome {
    /// 实际定位到 Cookies 数据库并读取的 profile 目录名
    profiles_scanned: Vec<String>,
    /// 各 profile host 匹配行数之和
    matched: usize,
    /// skip 细分计数（各 profile 求和）
    skips: SkipStats,
    /// (host_key, name) 去重合并后的 cookie
    cookies: Vec<RawCookie>,
}

/// 逐 profile 扫描：定位 DB（同一 DB 路径只读一次——根目录布局会被多个
/// profile 的候选命中）→ 副本读取 → host 过滤 + 解密 → (host, name) 合并。
///
/// 失败策略：TCC 权限拒绝立即整体失败（不重试）；单个 profile 的其他失败
/// （如 DB 损坏 / 锁）跳过、不影响其余 profile；一个都没读成才返回记录到的
/// 首个错误。任何 DB 都未命中时，若 `Local State` 也被权限拒绝则报 TCC
/// （强信号），否则报 `E_BROWSER_DATA_MISSING`。
fn scan_all_profiles(
    base: &Path,
    scan: &ProfileScan,
    browser_name: &str,
    key: &[u8; 16],
    site_hosts: &[String],
) -> Result<ScanOutcome, ImportError> {
    let mut out = ScanOutcome {
        profiles_scanned: Vec::new(),
        matched: 0,
        skips: SkipStats::default(),
        cookies: Vec::new(),
    };
    let mut seen_dbs: HashSet<PathBuf> = HashSet::new();
    let mut seen_keys: HashSet<(String, String)> = HashSet::new();
    let mut read_ok_any = false;
    let mut first_err: Option<ImportError> = None;

    for profile in &scan.profiles {
        let Some(db) = find_cookie_db(base, profile) else {
            ilog!("profile {profile}: no cookies db candidate found");
            continue;
        };
        if !seen_dbs.insert(db.clone()) {
            continue;
        }
        out.profiles_scanned.push(profile.clone());
        ilog!("profile {profile}: db {}", db.display());
        let (conn, _guard) = match open_cookie_copy(&db, browser_name, scan.local_state_denied) {
            Ok(v) => v,
            // TCC：一次失败即整体返回
            Err(e) if e.code == E_BROWSER_DATA_TCC_DENIED => return Err(e),
            // 单 profile 失败不影响其他 profile
            Err(e) => {
                ilog!("profile {profile}: open copy failed: {}", e.message);
                first_err.get_or_insert(e);
                continue;
            }
        };
        match read_and_decrypt(&conn, key, site_hosts) {
            Ok((matched, skips, cookies)) => {
                read_ok_any = true;
                out.matched += matched;
                out.skips.expired += skips.expired;
                out.skips.utf8 += skips.utf8;
                out.skips.decrypt += skips.decrypt;
                out.skips.read += skips.read;
                out.skips.expires_unknown += skips.expires_unknown;
                ilog!(
                    "profile {profile}: matched={matched} kept={} \
                     skipped_expired={} skipped_utf8={} skipped_decrypt={} skipped_read={} \
                     expires_unknown={}",
                    cookies.len(),
                    skips.expired,
                    skips.utf8,
                    skips.decrypt,
                    skips.read,
                    skips.expires_unknown
                );
                for c in cookies {
                    push_dedup(&mut out.cookies, &mut seen_keys, c);
                }
            }
            Err(e) => {
                ilog!("profile {profile}: read failed: {}", e.message);
                first_err.get_or_insert(e);
            }
        }
    }

    if out.profiles_scanned.is_empty() {
        return Err(if scan.local_state_denied {
            // Local State 与全部候选 DB 都探测不到内容，权限拒绝是强信号
            tcc_denied(browser_name, "Local State 与 Cookies 数据库")
        } else {
            ImportError::new(
                E_BROWSER_DATA_MISSING,
                format!(
                    "未找到 {} 的 Cookies 数据库（已按 profile 依次探测 {} 下各 profile 的 \
                     Cookies / Network/Cookies 与数据根目录候选；可能尚未在任何 profile \
                     登录过站点）",
                    browser_name,
                    base.display()
                ),
            )
        });
    }
    if !read_ok_any {
        // 命中过 DB 但全部读取失败：报首个具体错误
        return Err(first_err.unwrap_or_else(|| {
            ImportError::new(E_BROWSER_DATA_MISSING, "Cookies 数据库读取失败")
        }));
    }
    Ok(out)
}

// ---------- localStorage 导入（kimi 等不走 cookie 认证的站点） ----------
//
// 实证结论（2026-10-25，本机 Edge 156 + Chromium master 源码双重确认，
// 见 design.md §2.5.2）：Next.js SPA 类站点（kimi）登录 token 不在 cookie，
// 在 localStorage（LevelDB 键 `_https://www.kimi.com\0\x01access_token`）。
// 站点通过 sites.toml 的 `import_local_storage = true` 逐站点开启本路径，
// 与 cookie 流程共用浏览器检测 / profile 枚举 / TCC 分类 / 授权模型。

/// 站点 URL → localStorage origin（`scheme://host[:port]`，非默认端口才带）。
/// LevelDB 键按 origin 精确前缀匹配（www 与裸域是不同 origin，不做子域放宽）
fn origin_of(url: &url::Url) -> Option<String> {
    let host = url.host_str()?;
    Some(match url.port() {
        Some(p) => format!("{}://{host}:{p}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    })
}

/// localStorage leveldb 目录候选（有序）：profile 布局优先，数据根兜底
/// （Opera 无 profile 子目录，localStorage 挂在数据根下，同 Cookies 布局差异）
fn ls_db_candidates(base: &Path, profile: &str) -> Vec<PathBuf> {
    vec![
        base.join(profile)
            .join("Local Storage")
            .join("leveldb"),
        base.join("Local Storage").join("leveldb"),
    ]
}

/// 逐 profile 扫描 localStorage，提取各站点 origin 下的键值。
/// - 复用 cookie 流程的 profile 枚举（`Local State` 的 last_used 优先）；
///   实测 kimi token 位于非 Default profile，多 profile 扫描是硬要求；
/// - 同一目录路径只读一次（数据根布局会被多个 profile 的候选命中）；
/// - 直接读源文件（不复制）：LevelDB 是追加型日志结构，只读解析器容忍
///   torn write，读不到的最后几条由浏览器下次写入补齐，无损坏风险；
/// - 键按站点去重，先扫到的 profile 优先（last_used 在前，与 cookie 一致）；
/// - 权限拒绝（TCC）→ 整体失败（与 cookie 路径同一错误码）；目录缺失 /
///   其他 IO 错误 → 记日志跳过该目录；
/// - 数据安全：value 永不落日志（FAMA_DEBUG_IMPORT 下也只打键名+长度）。
///
/// 返回（site_id → 该站点去重后的条目，总条数）。
fn scan_local_storage(
    base: &Path,
    scan: &ProfileScan,
    browser_name: &str,
    ls_targets: &[&SiteConfig],
) -> Result<(HashMap<String, Vec<LsEntry>>, usize), ImportError> {
    let targets: Vec<(&str, String)> = ls_targets
        .iter()
        .filter_map(|t| {
            origin_of(&t.url).map(|o| (t.id.as_str(), o))
        })
        .collect();
    if targets.is_empty() {
        return Ok((HashMap::new(), 0));
    }

    let mut out: HashMap<String, Vec<LsEntry>> = HashMap::new();
    let mut seen_keys: HashMap<&str, HashSet<String>> = HashMap::new();
    let mut seen_dirs: HashSet<PathBuf> = HashSet::new();

    for profile in &scan.profiles {
        for dir in ls_db_candidates(base, profile) {
            if !dir.is_dir() || !seen_dirs.insert(dir.clone()) {
                continue;
            }
            ilog!("ls: profile {profile}: leveldb {}", dir.display());
            for (site_id, origin) in &targets {
                match local_storage_db::read_origin_entries(&dir, origin) {
                    Ok(entries) => {
                        let site_entries = out.entry(site_id.to_string()).or_default();
                        let seen = seen_keys.entry(site_id).or_default();
                        for e in entries {
                            if seen.insert(e.key.clone()) {
                                site_entries.push(e);
                            }
                        }
                    }
                    // TCC：一次失败即整体返回（与 cookie 路径同语义）
                    Err(local_storage_db::LsError::Denied(e)) => {
                        ilog!("ls: {site_id}: read {origin} denied: {e}");
                        return Err(tcc_denied(browser_name, "localStorage 数据"));
                    }
                    Err(local_storage_db::LsError::Io(e)) => {
                        ilog!("ls: {site_id}: read {origin} failed: {e}");
                    }
                    // Missing = 该 profile 无 localStorage 目录内容，正常跳过
                    Err(local_storage_db::LsError::Missing) => {}
                }
            }
        }
    }

    let mut total = 0;
    for (site_id, origin) in &targets {
        let entries = out.get(*site_id).map(|v| v.len()).unwrap_or(0);
        total += entries;
        // 永不打 value；键名+长度只在 FAMA_DEBUG_IMPORT 下打印（截断防刷屏）
        if debug_import() {
            if let Some(list) = out.get(*site_id) {
                let mut parts: Vec<String> = list
                    .iter()
                    .take(50)
                    .map(|e| format!("{}({})", e.key, e.value.len()))
                    .collect();
                if list.len() > 50 {
                    parts.push(format!("…共 {} 条", list.len()));
                }
                ilog!("ls: site={site_id} origin={origin} keys=[{}]", parts.join(", "));
            }
        }
    }
    ilog!(
        "ls total: {}",
        targets
            .iter()
            .map(|(id, _)| format!("{id}={}", out.get(*id).map(|v| v.len()).unwrap_or(0)))
            .collect::<Vec<_>>()
            .join(" ")
    );
    Ok((out, total))
}

/// 生成在站点页面上下文执行的注入脚本：批量 `localStorage.setItem`。
/// - 首行 `// entries: N` 注释供 tabs::ls_scripts_entry_count 计数
///   （eval 无同步返回值，注入条数以此统计进 ImportReport）；
/// - 键值经 serde_json 转义后内嵌，任意字符（引号/换行/非 ASCII）安全；
/// - 每条 setItem 独立 try/catch，单键失败（如配额）不中断其余；
/// - 注入是否生效由后续 reload + 页面登录态体现，报告/前端可见非静默。
fn build_ls_inject_js(entries: &[LsEntry]) -> String {
    let pairs: Vec<(&str, &str)> = entries
        .iter()
        .map(|e| (e.key.as_str(), e.value.as_str()))
        .collect();
    let json = serde_json::to_string(&pairs).unwrap_or_else(|_| "[]".to_string());
    format!(
        "// entries: {}\n(function(){{try{{var d={};\
for(var i=0;i<d.length;i++){{try{{localStorage.setItem(d[i][0],d[i][1])}}catch(e){{}}}}\
}}catch(e){{}}}})();",
        entries.len(),
        json
    )
}

/// FAMA_DEBUG_IMPORT=1 的注入回读验证探针：把「注入的 t 个键中现存 p 个」
/// 写进 URL fragment（SPA 改 hash 不触发导航，Rust 侧轮询 wv.url() 读回）。
/// 只回读计数，**永不回读 value**。
fn ls_verify_probe_js(keys: &[String]) -> String {
    let json = serde_json::to_string(keys).unwrap_or_else(|_| "[]".to_string());
    format!(
        "(function(){{try{{var ks={json};var p=0,t=0;\
for(var i=0;i<ks.length;i++){{t++;if(localStorage.getItem(ks[i])!==null)p++;}}\
history.replaceState(null,'','#fama-ls-verify='+p+'/'+t);}}catch(e){{}}}})();"
    )
}

/// 注入回读验证（仅 FAMA_DEBUG_IMPORT=1；kimi 是 SPA，登录与否不改变 URL，
/// 这是不依赖 URL 的更强证据）：延迟等 reload / 首载完成，eval 探针后轮询
/// fragment。两轮尝试覆盖页面加载慢的情况；任何失败只打日志，不影响导入
/// 结果（线程 detached，进程正常生命周期内完成）。
fn spawn_ls_verify(app: AppHandle<Wry>, ls_keys: Vec<(String, Vec<String>)>) {
    std::thread::spawn(move || {
        let tabs = app.state::<Tabs>();
        for (site_id, keys) in &ls_keys {
            let mut verified = false;
            for attempt in 0..2 {
                // 每轮先等 reload/首载推进（首轮 eval 可能被导航吞掉，次轮兜底）
                std::thread::sleep(Duration::from_secs(6));
                if !tabs.eval_probe_if_loaded(site_id, &ls_verify_probe_js(keys)) {
                    ilog!(
                        "ls verify: {site_id}: attempt {}: 页面未就绪或 eval 失败",
                        attempt + 1
                    );
                    continue;
                }
                let deadline = Instant::now() + Duration::from_secs(4);
                loop {
                    if let Some(f) = tabs.url_fragment(&app, site_id) {
                        if let Some(rest) = f.strip_prefix("fama-ls-verify=") {
                            ilog!(
                                "ls verify: {site_id}: present={rest}                                  （reload 后 webview localStorage 回读计数）"
                            );
                            verified = true;
                            break;
                        }
                    }
                    if Instant::now() >= deadline {
                        ilog!("ls verify: {site_id}: attempt {}: 回读超时", attempt + 1);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(150));
                }
                if verified {
                    break;
                }
            }
            if !verified {
                ilog!("ls verify: {site_id}: 未能回读（页面加载慢或站点改写了 hash）");
            }
        }
    });
}

// ---------- 注入 WKHTTPCookieStore ----------

/// 把一条 RawCookie 转成 NSHTTPCookie；返回 None 表示属性无法构造。
/// HttpOnly / SameSite 用 Foundation 的私有属性键（wry 同款做法，见
/// wry::wkwebview::cookie_into_wkwebview 的注释与引用）。
unsafe fn build_ns_cookie(
    c: &RawCookie,
) -> Option<objc2::rc::Retained<objc2_foundation::NSHTTPCookie>> {
    use objc2::runtime::AnyObject;
    use objc2_foundation::{
        ns_string, NSDate, NSMutableDictionary, NSHTTPCookie, NSString, NSURL,
    };

    let name = NSString::from_str(&c.name);
    let value = NSString::from_str(&c.value);
    let path = NSString::from_str(if c.path.is_empty() { "/" } else { &c.path });

    let props: objc2::rc::Retained<NSMutableDictionary<NSString, AnyObject>> =
        NSMutableDictionary::from_slices(
            &[
                ns_string!("Name"),
                ns_string!("Value"),
                ns_string!("Path"),
            ],
            &[
                &*name as &AnyObject,
                &*value as &AnyObject,
                &*path as &AnyObject,
            ],
        );
    // 域 cookie（host_key 带前导 '.'）用 Domain；host-only cookie 用 OriginURL，
    // 与浏览器的发送语义对齐（host_matches 的两条分支）
    if let Some(dom) = c.host_key.strip_prefix('.') {
        let d = NSString::from_str(&format!(".{dom}"));
        props.insert(ns_string!("Domain"), &*d as &AnyObject);
    } else {
        let origin = NSString::from_str(&format!("https://{}/", c.host_key));
        let url = NSURL::URLWithString(&origin)?;
        props.insert(ns_string!("OriginURL"), &*url as &AnyObject);
    }
    if c.has_expires && c.expires_utc != 0 {
        let dt = NSDate::dateWithTimeIntervalSince1970(chromium_utc_to_unix_secs(c.expires_utc));
        props.insert(ns_string!("Expires"), &*dt as &AnyObject);
        // Version 与 Expires 成对设置（对齐 wry cookie_into_wkwebview 的既有组合；
        // 会话 cookie 不设 Version）
        props.insert(ns_string!("Version"), &NSString::from_str("0") as &AnyObject);
    }
    if c.is_secure {
        props.insert(ns_string!("Secure"), ns_string!("TRUE") as &AnyObject);
    }
    if c.is_httponly {
        props.insert(ns_string!("HttpOnly"), ns_string!("TRUE") as &AnyObject);
    }
    let samesite = match c.samesite {
        1 => Some(ns_string!("lax")),
        2 => Some(ns_string!("strict")),
        _ => None, // -1 未指定 / 0 None：不设置
    };
    if let Some(ss) = samesite {
        props.insert(ns_string!("SameSite"), ss as &AnyObject);
    }
    NSHTTPCookie::cookieWithProperties(&props)
}

/// 向默认 data store 的 cookie store 注入全部 cookies。
/// 返回 (成功注入条数, 构造 NSHTTPCookie 失败而跳过的条数)。
/// setCookie 的 completion 由 WebKit 派发回主队列：本函数只在主线程发起调用，
/// 在调用线程（非主线程）同步等待全部 completion。
fn inject_cookies(
    app: &AppHandle<Wry>,
    cookies: Vec<RawCookie>,
) -> Result<(usize, usize), ImportError> {
    let (tx, rx) = mpsc::channel::<Result<(usize, usize), String>>();
    let total = cookies.len();
    app.run_on_main_thread(move || {
        let res = (|| -> Result<(), String> {
            use objc2_foundation::MainThreadMarker;
            use objc2_web_kit::WKWebsiteDataStore;

            // Safety: objc 运行时调用；当前在主线程
            unsafe {
                let mtm = MainThreadMarker::new().ok_or("不在主线程")?;
                let store = WKWebsiteDataStore::defaultDataStore(mtm);
                let cookie_store = store.httpCookieStore();
                let mut ns_cookies = Vec::with_capacity(total);
                for c in &cookies {
                    // 单条构造失败（如空 name）只跳过该条，不中止整体
                    if let Some(ns) = build_ns_cookie(c) {
                        ns_cookies.push(ns);
                    }
                }
                let skipped_build = total - ns_cookies.len();
                if ns_cookies.is_empty() {
                    let _ = tx.send(Ok((0, skipped_build)));
                    return Ok(());
                }
                let n = ns_cookies.len();
                let done = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
                for ns in ns_cookies {
                    let done = done.clone();
                    let tx = tx.clone();
                    let block = block2::RcBlock::new(move || {
                        if done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1 == n {
                            let _ = tx.send(Ok((n, skipped_build)));
                        }
                    });
                    cookie_store.setCookie_completionHandler(&ns, Some(&block));
                }
            }
            Ok(())
        })();
        if let Err(e) = res {
            let _ = tx.send(Err(e));
        }
    })
    .map_err(|e| ImportError::new(E_INJECT_FAILED, format!("派发主线程失败: {e}")))?;
    rx.recv_timeout(Duration::from_secs(10))
        .map_err(|_| ImportError::new(E_INJECT_FAILED, "cookie 注入超时"))?
        .map_err(|e| ImportError::new(E_INJECT_FAILED, e))
}

// ---------- 对外入口 ----------

/// 导入报告（IPC 契约，camelCase 序列化给前端）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportReport {
    /// 导入来源浏览器名
    pub browser: String,
    /// 首个扫描的浏览器 profile 目录名（兼容字段；完整列表见 profiles_scanned）
    pub profile: String,
    /// 实际定位到 Cookies 数据库并扫描的全部 profile 目录名
    pub profiles_scanned: Vec<String>,
    /// host 匹配到的 cookie 行数（跨 profile 求和）
    pub matched: usize,
    /// 成功注入的条数
    pub injected: usize,
    /// 解密失败（非 v10 / padding 错）跳过的条数
    pub skipped_decrypt: usize,
    /// 过期 / 非 UTF-8 等其他跳过
    pub skipped_other: usize,
    /// 注入后触发 reload 的站点 id
    pub reloaded: Vec<String>,
    /// localStorage 扫描读到的条目数（import_local_storage 站点，跨 profile 去重）
    pub ls_read: usize,
    /// 已加载页面上立即 eval 注入的 localStorage 条数
    pub ls_injected: usize,
    /// 挂起待站点首次加载完成注入的 localStorage 条数（含在 ls_read 内）
    pub ls_pending: usize,
}

/// 完整导入流程。手动导入（🔑）调用前提：用户已在前端确认框同意（第一道授权）；
/// Keychain ACL 弹窗（第二道授权）由系统在本函数读 Safe Storage 时弹出。
/// 启动自动导入走 [`import_auto`]（无确认框，Keychain 弹窗即用户感知点）。
pub fn import(app: &AppHandle<Wry>, targets: &[SiteConfig]) -> Result<ImportReport, ImportError> {
    if targets.is_empty() {
        return Err(ImportError::new(E_NO_SITES, "未选择任何有效站点"));
    }
    ilog!(
        "start: {} sites ({})",
        targets.len(),
        targets
            .iter()
            .map(|t| t.id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );

    // 1. 识别默认浏览器（不支持的浏览器在这里就返回，不触发 Keychain）
    let bundle_id = detect_default_browser_bundle_id(app)?;
    ilog!("default browser bundle id: {bundle_id}");
    let browser = browser_for_bundle_id(&bundle_id).ok_or_else(|| {
        ImportError::new(
            E_UNSUPPORTED_BROWSER,
            format!(
                "默认浏览器（bundle id {bundle_id}）不受支持；支持 Chrome / Edge / Brave / \
                 Vivaldi / Opera / Chromium，不支持 Safari 与 Firefox"
            ),
        )
    })?;

    // 2. Keychain 读取 + 派生 key（有本次运行缓存）
    let key = derived_key_for(browser)?;
    ilog!("keychain: derived cookie key ok");

    // 3. 定位用户数据目录（TCC 下元数据访问仍可用，is_dir 不会被拦截）
    let base = app
        .path()
        .home_dir()
        .map_err(|e| ImportError::new(E_BROWSER_DATA_MISSING, format!("取 home 目录失败: {e}")))?
        .join("Library/Application Support")
        .join(browser.data_dir);
    if !base.is_dir() {
        ilog!("data dir missing: {}", base.display());
        return Err(ImportError::new(
            E_BROWSER_DATA_MISSING,
            format!("未找到 {} 的用户数据目录 {}", browser.name, base.display()),
        ));
    }
    ilog!("data dir: {}", base.display());

    // 4. 多 profile 枚举 + 逐一扫描（副本查询 + host 过滤 + 解密 + 去重合并）。
    //    TCC 权限拒绝（Local State / Cookies 复制 EPERM）在这里以
    //    E_BROWSER_DATA_TCC_DENIED 一次失败即返回。
    let site_hosts: Vec<String> = targets
        .iter()
        .filter_map(|t| t.url.host_str().map(|h| h.to_string()))
        .collect();
    ilog!("site hosts: {}", site_hosts.join(", "));
    let scan = list_profiles(&base);
    ilog!(
        "profiles: {:?} (local_state_denied: {})",
        scan.profiles,
        scan.local_state_denied
    );
    let outcome = scan_all_profiles(&base, &scan, browser.name, &key, &site_hosts)?;
    ilog!(
        "scan total: matched={} kept={} skipped_expired={} skipped_utf8={} \
         skipped_decrypt={} skipped_read={} expires_unknown={}",
        outcome.matched,
        outcome.cookies.len(),
        outcome.skips.expired,
        outcome.skips.utf8,
        outcome.skips.decrypt,
        outcome.skips.read,
        outcome.skips.expires_unknown
    );
    if debug_import() {
        for c in &outcome.cookies {
            ilog!(
                "cookie host={} name={} secure={} httponly={} samesite={} expires_utc={} has_expires={}",
                c.host_key,
                c.name,
                c.is_secure,
                c.is_httponly,
                c.samesite,
                c.expires_utc,
                c.has_expires
            );
        }
    }

    // 5. 注入默认 data store（所有站点 webview 共享，见 design.md §2.5）
    let (injected, skipped_build) = inject_cookies(app, outcome.cookies)?;
    ilog!(
        "inject: set={} skipped_build={}",
        injected,
        skipped_build
    );
    let skipped_other = outcome.skips.other() + skipped_build;

    if outcome.matched > 0 && injected == 0 && outcome.skips.decrypt == outcome.matched {
        return Err(ImportError::new(
            E_DECRYPT_FAILED,
            format!(
                "匹配到 {} 条 cookie 但全部解密失败（浏览器可能启用了新的加密方案）",
                outcome.matched
            ),
        ));
    }

    // 6. localStorage 扫描（仅 import_local_storage 站点，kimi 等 SPA 的
    //    登录 token 在 localStorage 不在 cookie）。与 cookie 共用 profile
    //    枚举与 TCC 分类；读不到条目不算失败（可能该浏览器未登录过），
    //    数量进报告由前端可见呈现，不静默。
    let ls_targets: Vec<&SiteConfig> =
        targets.iter().filter(|t| t.import_local_storage).collect();
    let (ls_entries, ls_read) = if ls_targets.is_empty() {
        (HashMap::new(), 0)
    } else {
        ilog!("ls sites: {}", ls_targets.iter().map(|t| t.id.as_str()).collect::<Vec<_>>().join(", "));
        scan_local_storage(&base, &scan, browser.name, &ls_targets)?
    };

    // 7. reload 已加载的站点页面，让登录态立即生效；尚未完成首载的页面由
    //    Tabs 的 pending_reload / pending_ls 在首载完成时补注入+reload
    //    （导航请求可能早于注入发出，不带新登录态）。localStorage 站点
    //    先 eval setItem 再 reload，一次刷新同时生效 cookie 与 localStorage。
    let site_ids: Vec<String> = targets.iter().map(|t| t.id.clone()).collect();
    let ls_scripts: HashMap<String, String> = ls_entries
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(id, v)| (id.clone(), build_ls_inject_js(v)))
        .collect();
    let rl = app.state::<Tabs>().reload_loaded(&site_ids, &ls_scripts);
    ilog!(
        "reload now: {:?} (未就绪站点转入 pending_reload/pending_ls)",
        rl.reloaded
    );
    if !ls_scripts.is_empty() {
        ilog!(
            "ls inject: now={} pending={} (待站点首载完成)",
            rl.ls_injected_now, rl.ls_pending
        );
        // FAMA_DEBUG_IMPORT：延迟回读验证（只回读键存在计数，不回读 value）
        if debug_import() {
            let ls_keys: Vec<(String, Vec<String>)> = ls_entries
                .iter()
                .filter(|(_, v)| !v.is_empty())
                .map(|(id, v)| (id.clone(), v.iter().map(|e| e.key.clone()).collect()))
                .collect();
            spawn_ls_verify(app.clone(), ls_keys);
        }
    }

    ilog!(
        "done: browser={} profiles={:?} injected={} ls_read={} ls_injected={} ls_pending={}",
        browser.name,
        outcome.profiles_scanned,
        injected,
        ls_read,
        rl.ls_injected_now,
        rl.ls_pending
    );
    Ok(ImportReport {
        browser: browser.name.to_string(),
        profile: outcome
            .profiles_scanned
            .first()
            .cloned()
            .unwrap_or_else(|| "Default".to_string()),
        profiles_scanned: outcome.profiles_scanned,
        matched: outcome.matched,
        injected,
        skipped_decrypt: outcome.skips.decrypt,
        skipped_other,
        reloaded: rl.reloaded,
        ls_read,
        ls_injected: rl.ls_injected_now,
        ls_pending: rl.ls_pending,
    })
}

// ---------- 启动自动导入（每进程至多一次） ----------

/// 自动导入是否已在本进程尝试过的守卫。防止 dev 下前端热重载反复触发
/// （每次都会重新读浏览器 DB；未授权时反复弹 Keychain ACL 框）。
/// 手动导入（🔑 ImportDialog）不受此守卫限制，始终可重试。
static AUTO_IMPORT_ATTEMPTED: AtomicBool = AtomicBool::new(false);

/// check-and-set：返回 true 表示本进程已经自动导入过（本次调用被拒绝）。
fn auto_import_gate() -> bool {
    AUTO_IMPORT_ATTEMPTED.swap(true, Ordering::SeqCst)
}

/// 启动时自动导入（App mount 后前端触发）：与手动导入同流程，仅多一道
/// 每进程一次的守卫。已尝试过则返回 `E_AUTO_ALREADY_RUN`（前端静默忽略）。
/// 首次运行时 Keychain ACL 弹窗（若未「始终允许」）就是用户感知与授权点。
pub fn import_auto(
    app: &AppHandle<Wry>,
    targets: &[SiteConfig],
) -> Result<ImportReport, ImportError> {
    if auto_import_gate() {
        ilog!("auto: already attempted in this process, skipping");
        return Err(ImportError::new(
            E_AUTO_ALREADY_RUN,
            "本进程已执行过一次自动导入",
        ));
    }
    ilog!("auto: startup import begin");
    import(app, targets)
}

// ---------- 单元测试 ----------

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 6070 PBKDF2-HMAC-SHA1 向量：P="password" S="salt" c=1 dkLen=16
    #[test]
    fn pbkdf2_known_vector() {
        let mut out = [0u8; 16];
        pbkdf2_sha1(b"password", b"salt", 1, &mut out);
        assert_eq!(
            out,
            [
                0x0c, 0x60, 0xc8, 0x0f, 0x96, 0x1f, 0x0e, 0x71, 0xf3, 0xa9, 0xb5, 0x24, 0xaf,
                0x60, 0x12, 0x06
            ]
        );
    }

    #[test]
    fn derive_key_is_deterministic_16_bytes() {
        let a = derive_cookie_key(b"peanuts");
        let b = derive_cookie_key(b"peanuts");
        assert_eq!(*a, *b);
        assert_eq!(a.len(), 16);
        assert_ne!(*derive_cookie_key(b"peanut"), *a);
    }

    fn encrypt_v10(key: &[u8; 16], plaintext: &[u8]) -> Vec<u8> {
        use cbc::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
        type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;
        let iv = [b' '; 16];
        let enc = Aes128CbcEnc::new_from_slices(key, &iv).unwrap();
        let mut out = b"v10".to_vec();
        out.extend_from_slice(&enc.encrypt_padded_vec_mut::<Pkcs7>(plaintext));
        out
    }

    #[test]
    fn decrypt_v10_roundtrip() {
        let key = *derive_cookie_key(b"peanuts");
        for pt in [&b""[..], b"a", b"session-token-abcdef0123456789", &[0u8; 32]] {
            let ct = encrypt_v10(&key, pt);
            assert_eq!(decrypt_v10(&key, &ct).unwrap(), pt);
        }
    }

    #[test]
    fn decrypt_v10_rejects_bad_prefix() {
        let key = [0u8; 16];
        let mut ct = encrypt_v10(&key, b"hello");
        ct[1] = b'9'; // "v90"
        assert_eq!(decrypt_v10(&key, &ct), Err(DecryptError::NotV10));
        assert_eq!(decrypt_v10(&key, b"v20xxxx"), Err(DecryptError::NotV10));
    }

    #[test]
    fn decrypt_v10_rejects_bad_input() {
        let key = [1u8; 16];
        // 空体 / 非 16 倍数
        assert_eq!(decrypt_v10(&key, b"v10"), Err(DecryptError::BadLength));
        assert_eq!(decrypt_v10(&key, b"v10abc"), Err(DecryptError::BadLength));
        // 16 字节垃圾：解密后 PKCS7 去填充几乎必失败
        assert_eq!(
            decrypt_v10(&key, b"v10aaaaaaaaaaaaaaaa"),
            Err(DecryptError::BadPadding)
        );
    }

    #[test]
    fn bundle_id_mapping() {
        assert_eq!(
            browser_for_bundle_id("com.google.Chrome").unwrap().name,
            "Chrome"
        );
        assert_eq!(
            browser_for_bundle_id("com.microsoft.edgemac").unwrap().name,
            "Edge"
        );
        assert_eq!(
            browser_for_bundle_id("com.brave.Browser").unwrap().name,
            "Brave"
        );
        assert_eq!(
            browser_for_bundle_id("com.vivaldi.Vivaldi").unwrap().name,
            "Vivaldi"
        );
        assert_eq!(
            browser_for_bundle_id("com.operasoftware.Opera").unwrap().name,
            "Opera"
        );
        assert_eq!(
            browser_for_bundle_id("org.chromium.Chromium").unwrap().name,
            "Chromium"
        );
        assert!(browser_for_bundle_id("com.apple.Safari").is_none());
        assert!(browser_for_bundle_id("org.mozilla.firefox").is_none());
        assert!(browser_for_bundle_id("").is_none());
    }

    #[test]
    fn host_predicate() {
        // 域 cookie：子域与自身都匹配
        assert!(host_matches(".kimi.com", "www.kimi.com"));
        assert!(host_matches(".kimi.com", "kimi.com"));
        assert!(host_matches(".deepseek.com", "chat.deepseek.com"));
        assert!(host_matches(".chat.deepseek.com", "chat.deepseek.com"));
        // host-only cookie：仅精确匹配
        assert!(host_matches("chatgpt.com", "chatgpt.com"));
        assert!(!host_matches("chatgpt.com", "www.chatgpt.com"));
        // 不相关域
        assert!(!host_matches(".openai.com", "chatgpt.com"));
        assert!(!host_matches("chatglm.cn", "chatgpt.com"));
        assert!(!host_matches(".google.com", "chatgpt.com"));
        // 大小写与尾点
        assert!(host_matches(".Kimi.COM", "www.kimi.com."));
        assert!(!host_matches(".", ""));
    }

    #[test]
    fn epoch_conversion() {
        // 1601 epoch 与 Unix epoch 的偏移：11644473600s
        assert_eq!(chromium_utc_to_unix_secs(11_644_473_600_000_000), 0.0);
        // 2024-01-01T00:00:00Z = unix 1704067200
        assert_eq!(chromium_utc_to_unix_secs(13_348_540_800_000_000), 1_704_067_200.0);
    }

    /// Chromium cookies 表的最小真实 schema（read_and_decrypt 与 write_cookies_db 共用）
    const COOKIES_SCHEMA: &str = "CREATE TABLE cookies (
        creation_utc INTEGER NOT NULL,
        host_key TEXT NOT NULL,
        top_frame_site_key TEXT NOT NULL DEFAULT '',
        name TEXT NOT NULL,
        value TEXT NOT NULL DEFAULT '',
        encrypted_value BLOB DEFAULT '',
        path TEXT NOT NULL DEFAULT '/',
        expires_utc INTEGER NOT NULL DEFAULT 0,
        is_secure INTEGER NOT NULL DEFAULT 0,
        is_httponly INTEGER NOT NULL DEFAULT 0,
        last_access_utc INTEGER NOT NULL DEFAULT 0,
        has_expires INTEGER NOT NULL DEFAULT 1,
        is_persistent INTEGER NOT NULL DEFAULT 1,
        priority INTEGER NOT NULL DEFAULT 1,
        samesite INTEGER NOT NULL DEFAULT -1
    );";

    /// 用 Chromium 真实 schema 造一个 Cookies 库，端到端验证 read_and_decrypt：
    /// host 过滤、v10 解密、明文 value 回退、过期剔除。
    #[test]
    fn read_and_decrypt_end_to_end() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(COOKIES_SCHEMA).unwrap();

        let key = *derive_cookie_key(b"peanuts");
        // 2099 年（chromium epoch 微秒）作为"未过期"
        let far_future = (4_102_444_800 + 11_644_473_600) * 1_000_000;
        // 2000 年作为"已过期"
        let past = (946_684_800 + 11_644_473_600) * 1_000_000;
        let insert = |host: &str, name: &str, value: &str, enc: Vec<u8>, expires: i64| {
            conn.execute(
                "INSERT INTO cookies (creation_utc, host_key, name, value, encrypted_value, \
                 path, expires_utc, is_secure, is_httponly, has_expires, samesite) \
                 VALUES (0, ?1, ?2, ?3, ?4, '/', ?5, 1, 1, 1, 1)",
                rusqlite::params![host, name, value, enc, expires],
            )
            .unwrap();
        };
        insert(
            ".kimi.com",
            "session",
            "",
            encrypt_v10(&key, b"tok-abc"),
            far_future,
        );
        // 明文 value（encrypted 为空）也要能用
        insert(".kimi.com", "pref", "zh", vec![], far_future);
        // 不相关域：不导入
        insert(".example.com", "x", "y", vec![], far_future);
        // 过期：剔除并计 skipped_other
        insert(".kimi.com", "old", "z", vec![], past);
        // 非 v10：计入 skipped_decrypt
        insert(".kimi.com", "future", "", b"v20deadbeef".to_vec(), far_future);

        let hosts = vec!["www.kimi.com".to_string(), "chatgpt.com".to_string()];
        let (matched, skips, cookies) = read_and_decrypt(&conn, &key, &hosts).unwrap();

        assert_eq!(matched, 4); // .kimi.com 的 4 条（不含 example.com）
        assert_eq!(skips.decrypt, 1); // v20
        assert_eq!(skips.expired, 1); // 2000 年
        assert_eq!(skips.utf8, 0);
        assert_eq!(skips.read, 0);
        assert_eq!(skips.expires_unknown, 0);
        assert_eq!(skips.other(), 1);
        assert_eq!(cookies.len(), 2);
        let session = cookies.iter().find(|c| c.name == "session").unwrap();
        assert_eq!(session.value, "tok-abc");
        assert_eq!(session.host_key, ".kimi.com");
        assert!(session.is_secure && session.is_httponly);
        assert_eq!(session.samesite, 1);
        let pref = cookies.iter().find(|c| c.name == "pref").unwrap();
        assert_eq!(pref.value, "zh");
    }

    // ---------- Cookies DB 候选路径解析 ----------

    /// 宽松 i64 读取的各值形态：Integer 直取；整值 Real / 数字 Text 可解释；
    /// NULL / 小数 Real / 非数字 Text / Blob → None
    #[test]
    fn value_ref_as_i64_accepts_numeric_forms() {
        use rusqlite::types::ValueRef;
        assert_eq!(value_ref_as_i64(ValueRef::Integer(42)), Some(42));
        assert_eq!(value_ref_as_i64(ValueRef::Integer(0)), Some(0));
        assert_eq!(value_ref_as_i64(ValueRef::Real(1.7e16)), Some(17_000_000_000_000_000));
        assert_eq!(value_ref_as_i64(ValueRef::Real(1.5)), None);
        assert_eq!(value_ref_as_i64(ValueRef::Text(b"123")), Some(123));
        assert_eq!(value_ref_as_i64(ValueRef::Text(b" 456 ")), Some(456));
        assert_eq!(value_ref_as_i64(ValueRef::Text(b"abc")), None);
        assert_eq!(value_ref_as_i64(ValueRef::Null), None);
        assert_eq!(value_ref_as_i64(ValueRef::Blob(b"123")), None);
    }

    /// expires_utc 无有效数值的 cookie 不再被 unwrap_or(0) 换算成 1601 年附近
    /// 而整库误判过期：按会话 cookie 保留，计入 expires_unknown（真凶定位计数）。
    /// 覆盖两种形态：expires_utc=0 且 has_expires=1 的矛盾行（schema NOT NULL，
    /// Edge 156 最大嫌疑）、非数字 TEXT（类型不符）。
    #[test]
    fn read_and_decrypt_keeps_cookie_with_unreadable_expires() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(COOKIES_SCHEMA).unwrap();
        let key = *derive_cookie_key(b"peanuts");
        let insert = |name: &str, expires: rusqlite::types::Value| {
            conn.execute(
                "INSERT INTO cookies (creation_utc, host_key, name, value, encrypted_value, \
                 path, expires_utc, is_secure, is_httponly, has_expires, samesite) \
                 VALUES (0, '.kimi.com', ?1, 'v', x'', '/', ?2, 0, 0, 1, -1)",
                rusqlite::params![name, expires],
            )
            .unwrap();
        };
        insert("zero-exp", rusqlite::types::Value::Integer(0));
        insert("text-exp", rusqlite::types::Value::Text("not-a-number".to_string()));
        // 正常会话 cookie（0 + has_expires=0）：保留但不计 expires_unknown
        conn.execute(
            "INSERT INTO cookies (creation_utc, host_key, name, value, encrypted_value, \
             path, expires_utc, is_secure, is_httponly, has_expires, samesite) \
             VALUES (0, '.kimi.com', 'session-ok', 'v', x'', '/', 0, 0, 0, 0, -1)",
            [],
        )
        .unwrap();

        let (matched, skips, cookies) =
            read_and_decrypt(&conn, &key, &["www.kimi.com".to_string()]).unwrap();
        assert_eq!(matched, 3);
        assert_eq!(skips.expired, 0); // 不再误判过期
        assert_eq!(skips.expires_unknown, 2); // 矛盾行 + TEXT；正常会话不计
        assert_eq!(cookies.len(), 3); // 全部保留
        for c in &cookies {
            assert!(!c.has_expires); // 会话 cookie 语义
            assert_eq!(c.expires_utc, 0);
        }
    }

    /// 解密成功但明文非 UTF-8 → 计入 utf8 skip
    #[test]
    fn read_and_decrypt_skips_non_utf8_plaintext() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(COOKIES_SCHEMA).unwrap();
        let key = *derive_cookie_key(b"peanuts");
        conn.execute(
            "INSERT INTO cookies (creation_utc, host_key, name, value, encrypted_value, \
             path, expires_utc, is_secure, is_httponly, has_expires, samesite) \
             VALUES (0, '.kimi.com', 'bin', '', ?1, '/', 0, 0, 0, 0, -1)",
            rusqlite::params![encrypt_v10(&key, &[0xff, 0xfe, 0xfd, 0xfc])],
        )
        .unwrap();
        let (matched, skips, cookies) =
            read_and_decrypt(&conn, &key, &["www.kimi.com".to_string()]).unwrap();
        assert_eq!(matched, 1);
        assert_eq!(skips.utf8, 1);
        assert_eq!(skips.decrypt, 0); // 解密本身成功
        assert!(cookies.is_empty());
    }

    // ---------- Edge 156+ domain hash 前缀 ----------

    /// SHA-256(host_key) 前缀的校验/剥离：匹配 → 剥掉；不匹配 / 太短 → None
    #[test]
    fn strip_domain_hash_prefix_matches_only_host_hash() {
        use sha2::{Digest, Sha256};
        let mut blob = Sha256::digest(b".kimi.com").to_vec();
        blob.extend_from_slice(b"tok-xyz");
        assert_eq!(
            strip_domain_hash_prefix(&blob, ".kimi.com"),
            Some(b"tok-xyz".as_slice())
        );
        // host_key 不同（含前导点差异）→ hash 不匹配
        assert_eq!(strip_domain_hash_prefix(&blob, "kimi.com"), None);
        // 旧格式明文（无前缀）→ 不剥
        assert_eq!(strip_domain_hash_prefix(b"plain-value", ".kimi.com"), None);
        // 短于 32 字节 → 不剥
        assert_eq!(strip_domain_hash_prefix(&blob[..31], ".kimi.com"), None);
        // 恰好 32 字节（hash + 空值）→ 剥成空串
        assert_eq!(
            strip_domain_hash_prefix(&blob[..32], ".kimi.com"),
            Some(b"".as_slice())
        );
    }

    /// Edge 156+ 真实格式端到端：v10 密文 = AES(SHA-256(host_key) + value)，
    /// 解密后剥前缀得到正确 value；旧格式（无前缀）行同库共存互不影响
    #[test]
    fn read_and_decrypt_strips_edge156_domain_hash() {
        use sha2::{Digest, Sha256};
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(COOKIES_SCHEMA).unwrap();
        let key = *derive_cookie_key(b"peanuts");
        let mut blob = Sha256::digest(b".kimi.com").to_vec();
        blob.extend_from_slice(b"tok-edge156");
        let insert = |name: &str, enc: Vec<u8>| {
            conn.execute(
                "INSERT INTO cookies (creation_utc, host_key, name, value, encrypted_value, \
                 path, expires_utc, is_secure, is_httponly, has_expires, samesite) \
                 VALUES (0, '.kimi.com', ?1, '', ?2, '/', 0, 0, 0, 0, -1)",
                rusqlite::params![name, enc],
            )
            .unwrap();
        };
        insert("edge156", encrypt_v10(&key, &blob));
        insert("legacy", encrypt_v10(&key, b"tok-legacy"));

        let (matched, skips, cookies) =
            read_and_decrypt(&conn, &key, &["www.kimi.com".to_string()]).unwrap();
        assert_eq!(matched, 2);
        assert_eq!(skips.utf8, 0);
        assert_eq!(skips.decrypt, 0);
        assert_eq!(cookies.len(), 2);
        assert_eq!(
            cookies.iter().find(|c| c.name == "edge156").unwrap().value,
            "tok-edge156"
        );
        assert_eq!(
            cookies.iter().find(|c| c.name == "legacy").unwrap().value,
            "tok-legacy"
        );
    }


    /// 测试用临时目录（drop 时递归删除，不影响真实浏览器数据）
    struct TmpDir(PathBuf);
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn make_tmp_dir(tag: &str) -> TmpDir {
        let p = std::env::temp_dir().join(format!(
            "fama-test-cookies-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        std::fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }

    /// 创建父目录并写入一个字节（模拟已存在的 Cookies 文件）
    fn touch(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"x").unwrap();
    }

    #[test]
    fn cookie_db_candidates_order() {
        let base = Path::new("/fake/base");
        let got = cookie_db_candidates(base, "Profile 1");
        assert_eq!(
            got,
            vec![
                PathBuf::from("/fake/base/Profile 1/Cookies"),
                PathBuf::from("/fake/base/Profile 1/Network/Cookies"),
                PathBuf::from("/fake/base/Cookies"),
                PathBuf::from("/fake/base/Network/Cookies"),
            ]
        );
    }

    // ---------- localStorage 导入 ----------

    /// origin_of：默认端口省略、显式端口保留；与 LevelDB 键的前缀精确匹配
    #[test]
    fn origin_of_forms_exact_origin() {
        let u = url::Url::parse("https://www.kimi.com/").unwrap();
        assert_eq!(origin_of(&u).as_deref(), Some("https://www.kimi.com"));
        let u = url::Url::parse("https://example.com:8443/chat").unwrap();
        assert_eq!(origin_of(&u).as_deref(), Some("https://example.com:8443"));
        let u = url::Url::parse("about:blank").unwrap();
        assert_eq!(origin_of(&u), None);
    }

    /// ls 目录候选：profile 布局在前、数据根（Opera 布局）兜底
    #[test]
    fn ls_db_candidates_order() {
        let base = Path::new("/fake/base");
        assert_eq!(
            ls_db_candidates(base, "Profile 1"),
            vec![
                PathBuf::from("/fake/base/Profile 1/Local Storage/leveldb"),
                PathBuf::from("/fake/base/Local Storage/leveldb"),
            ]
        );
    }

    /// 注入脚本：首行条目计数头（tabs::ls_scripts_entry_count 的依据）+
    /// 键值 JSON 转义（引号/换行/`</script>` 等任意字符安全内嵌，可原样
    /// 解析回原值）
    #[test]
    fn build_ls_inject_js_escapes_and_counts() {
        let entries = vec![
            LsEntry {
                key: "access_token".into(),
                value: "v\"1\n2\t3".into(),
            },
            LsEntry {
                key: "k</script>".into(),
                value: "plain".into(),
            },
        ];
        let js = build_ls_inject_js(&entries);
        assert!(js.starts_with("// entries: 2\n"));
        assert!(js.contains("localStorage.setItem"));
        // 从 `var d=` 之后流式解析第一个 JSON 值（不能找 `]`——脚本里
        // `d[i][1]` 也含方括号）
        let marker = "var d=";
        let rest = &js[js.find(marker).unwrap() + marker.len()..];
        let parsed: Vec<(String, String)> =
            serde_json::Deserializer::from_str(rest)
                .into_iter()
                .next()
                .unwrap()
                .unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(
            parsed[0],
            ("access_token".to_string(), "v\"1\n2\t3".to_string())
        );
        assert_eq!(parsed[1], ("k</script>".to_string(), "plain".to_string()));
    }

    /// localStorage 测试站点配置（kimi 同形）
    fn ls_kimi_site() -> SiteConfig {
        SiteConfig {
            id: "kimi".into(),
            name: "Kimi".into(),
            url: url::Url::parse("https://www.kimi.com/").unwrap(),
            icon: "kimi".into(),
            injector_src: "",
            injector_ver: "0".into(),
            import_local_storage: true,
        }
    }

    /// scan_local_storage 端到端（手工构造 leveldb SSTable）：
    /// - 跨 profile 扫描（kimi token 实测在非 Default profile）；
    /// - last_used profile 优先（同键先扫到者胜，与 cookie 去重一致）；
    /// - origin 精确过滤（他站键与近似域名 `www.kimi.com.evil.com` 不混入）；
    /// - Latin1 与 UTF-16 两种编码的值都解码。
    #[test]
    fn scan_local_storage_end_to_end() {
        use crate::local_storage_db::test_support as ls;

        let tmp = make_tmp_dir("ls-e2e");
        let base = tmp.0.join("Edge");
        let p1 = base.join("Profile 1/Local Storage/leveldb");
        let def = base.join("Default/Local Storage/leveldb");
        std::fs::create_dir_all(&p1).unwrap();
        std::fs::create_dir_all(&def).unwrap();

        // Profile 1（last_used）：kimi 主数据 + 他站键 + 近似域名键（后两者应被过滤）
        let sstable_p1 = ls::build_sstable(&[vec![
            (
                ls::ls_key("https://www.kimi.com", "access_token"),
                ls::ls_encode_latin1("tok-profile1"),
            ),
            (
                ls::ls_key("https://www.kimi.com", "msh_user_id"),
                ls::ls_encode_latin1("u1"),
            ),
            (
                ls::ls_key("https://chat.deepseek.com", "x"),
                ls::ls_encode_latin1("other-site"),
            ),
            (
                ls::ls_key("https://www.kimi.com.evil.com", "y"),
                ls::ls_encode_latin1("evil"),
            ),
        ]]);
        std::fs::write(p1.join("000001.ldb"), sstable_p1).unwrap();

        // Default：同键旧值（Profile 1 先扫、应胜出）+ Default 独有键（UTF-16 值）
        let sstable_def = ls::build_sstable(&[vec![
            (
                ls::ls_key("https://www.kimi.com", "access_token"),
                ls::ls_encode_latin1("tok-default-older"),
            ),
            (
                ls::ls_key("https://www.kimi.com", "theme"),
                ls::ls_encode_utf16("dark"),
            ),
        ]]);
        std::fs::write(def.join("000001.ldb"), sstable_def).unwrap();

        let scan = ProfileScan {
            profiles: vec!["Profile 1".to_string(), "Default".to_string()],
            local_state_denied: false,
        };
        let kimi = ls_kimi_site();
        let (entries, total) = scan_local_storage(&base, &scan, "Edge", &[&kimi]).unwrap();

        assert_eq!(total, 3);
        let got = &entries["kimi"];
        // read_origin_entries 内按 key 排序：access_token, msh_user_id, theme
        assert_eq!(got[0].key, "access_token");
        assert_eq!(got[0].value, "tok-profile1"); // last_used profile 先扫先得
        assert_eq!(got[1].key, "msh_user_id");
        assert_eq!(got[1].value, "u1");
        assert_eq!(got[2].key, "theme");
        assert_eq!(got[2].value, "dark"); // UTF-16 值解码
        // 他站 / 近似域名键未混入
        assert!(got
            .iter()
            .all(|e| !e.value.contains("other-site") && !e.value.contains("evil")));
    }

    /// 验证探针：只内嵌键名（无 value）、写 fama-ls-verify fragment
    #[test]
    fn ls_verify_probe_js_shape() {
        let js = ls_verify_probe_js(&["access_token".to_string(), "msh_user_id".to_string()]);
        assert!(js.contains(r#"["access_token","msh_user_id"]"#));
        assert!(js.contains("#fama-ls-verify='+p+'/'+t"));
        assert!(js.contains("localStorage.getItem"));
    }

    /// 浏览器从未用过 localStorage（目录缺失）不算错误：空结果、零条目
    #[test]
    fn scan_local_storage_missing_dirs_is_ok() {
        let tmp = make_tmp_dir("ls-missing");
        let base = tmp.0.join("Edge");
        std::fs::create_dir_all(&base).unwrap();
        let scan = ProfileScan {
            profiles: vec!["Default".to_string()],
            local_state_denied: false,
        };
        let kimi = ls_kimi_site();
        let (entries, total) = scan_local_storage(&base, &scan, "Edge", &[&kimi]).unwrap();
        assert_eq!(total, 0);
        assert!(entries.is_empty());
    }

    /// Chrome 式布局：Cookies 在 profile 子目录下（新旧两种位置）
    #[test]
    fn find_cookie_db_chrome_layout() {
        // 旧版：base/{profile}/Cookies
        let tmp = make_tmp_dir("chrome-old");
        let base = &tmp.0;
        touch(&base.join("Default/Cookies"));
        assert_eq!(
            find_cookie_db(base, "Default"),
            Some(base.join("Default/Cookies"))
        );

        // 新版：base/{profile}/Network/Cookies
        let tmp = make_tmp_dir("chrome-new");
        let base = &tmp.0;
        touch(&base.join("Default/Network/Cookies"));
        assert_eq!(
            find_cookie_db(base, "Default"),
            Some(base.join("Default/Network/Cookies"))
        );
    }

    /// Opera 式布局：Cookies 在数据根目录下，无 profile 子目录
    #[test]
    fn find_cookie_db_opera_layout() {
        // 根下直接 Cookies
        let tmp = make_tmp_dir("opera-root");
        let base = &tmp.0;
        touch(&base.join("Cookies"));
        assert_eq!(find_cookie_db(base, "Default"), Some(base.join("Cookies")));

        // 根下 Network/Cookies
        let tmp = make_tmp_dir("opera-network");
        let base = &tmp.0;
        touch(&base.join("Network/Cookies"));
        assert_eq!(
            find_cookie_db(base, "Default"),
            Some(base.join("Network/Cookies"))
        );
    }

    /// profile 目录与数据根同时有候选时，按声明顺序取优先级最高的
    #[test]
    fn find_cookie_db_prefers_profile_then_plain_over_network() {
        let tmp = make_tmp_dir("prio");
        let base = &tmp.0;
        // profile/Cookies 优先于 profile/Network/Cookies 与根下所有候选
        touch(&base.join("Default/Cookies"));
        touch(&base.join("Default/Network/Cookies"));
        touch(&base.join("Cookies"));
        touch(&base.join("Network/Cookies"));
        assert_eq!(
            find_cookie_db(base, "Default"),
            Some(base.join("Default/Cookies"))
        );

        // profile 下只有 Network/Cookies 时，仍优先于根下的 Cookies
        let tmp = make_tmp_dir("prio2");
        let base = &tmp.0;
        touch(&base.join("Default/Network/Cookies"));
        touch(&base.join("Cookies"));
        assert_eq!(
            find_cookie_db(base, "Default"),
            Some(base.join("Default/Network/Cookies"))
        );
    }

    /// 目录存在但没有任何候选文件（浏览器装了但没登录过）→ None
    #[test]
    fn find_cookie_db_missing_returns_none() {
        let tmp = make_tmp_dir("empty");
        // 只建同名目录不建文件：目录不算候选
        std::fs::create_dir_all(tmp.0.join("Default/Cookies")).unwrap();
        assert_eq!(find_cookie_db(&tmp.0, "Default"), None);
    }

    // ---------- TCC / 权限拒绝错误分类 ----------

    /// EPERM(1) / EACCES(13) / 显式 PermissionDenied → E_BROWSER_DATA_TCC_DENIED；
    /// 其他 io 错误仍归 E_COOKIE_DB_LOCKED 并保留 action 上下文
    #[test]
    fn browser_read_err_maps_permission_denied_to_tcc() {
        for raw in [1, 13] {
            let e = std::io::Error::from_raw_os_error(raw);
            assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied);
            let err = browser_read_err(&e, "Edge", "复制 Cookies 数据库失败", false);
            assert_eq!(err.code, E_BROWSER_DATA_TCC_DENIED);
        }
        let e = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let err = browser_read_err(&e, "Edge", "复制 Cookies 数据库失败", false);
        assert_eq!(err.code, E_BROWSER_DATA_TCC_DENIED);

        let err = browser_read_err(
            &std::io::Error::from_raw_os_error(2),
            "Edge",
            "复制 Cookies 数据库失败",
            false,
        );
        assert_eq!(err.code, E_COOKIE_DB_LOCKED);
        assert!(err.message.contains("复制 Cookies 数据库失败"));
    }

    /// TCC 提示必须带分步授权指引；Local State 同被拦截时合并提示
    #[test]
    fn tcc_denied_message_gives_actionable_guidance() {
        let err = tcc_denied("Edge", "Cookies 数据库");
        assert_eq!(err.code, E_BROWSER_DATA_TCC_DENIED);
        for kw in ["Edge", "TCC", "完全磁盘访问", "终端", "fama.app", "重启"] {
            assert!(
                err.message.contains(kw),
                "提示缺少关键词 {kw}：{}",
                err.message
            );
        }
        let err = browser_read_err(
            &std::io::Error::new(std::io::ErrorKind::PermissionDenied, "x"),
            "Edge",
            "复制 Cookies 数据库失败",
            true,
        );
        assert_eq!(err.code, E_BROWSER_DATA_TCC_DENIED);
        assert!(err.message.contains("Local State"));
    }

    // ---------- 多 profile 枚举 ----------

    #[test]
    fn profiles_from_local_state_orders_last_used_first() {
        // last_used 优先，其余按字典序
        let txt = r#"{"profile":{"last_used":"Profile 1","info":{"Profile 1":{},"Default":{}}}}"#;
        assert_eq!(
            profiles_from_local_state(txt).unwrap(),
            vec!["Profile 1".to_string(), "Default".to_string()]
        );
        // 无 last_used：字典序
        let txt = r#"{"profile":{"info":{"Profile 2":{},"Default":{}}}}"#;
        assert_eq!(
            profiles_from_local_state(txt).unwrap(),
            vec!["Default".to_string(), "Profile 2".to_string()]
        );
        // info 缺 "Default"：补上（惯例的默认 profile 目录）
        let txt = r#"{"profile":{"info":{"Profile 2":{}}}}"#;
        assert_eq!(
            profiles_from_local_state(txt).unwrap(),
            vec!["Default".to_string(), "Profile 2".to_string()]
        );
        // last_used 不在 info 集合里：忽略，不进列表
        let txt = r#"{"profile":{"last_used":"Ghost","info":{"Default":{}}}}"#;
        assert_eq!(
            profiles_from_local_state(txt).unwrap(),
            vec!["Default".to_string()]
        );
        // 坏 JSON / 无 profile.info / info 为空 → None（调用方回退探测）
        assert!(profiles_from_local_state("not json").is_none());
        assert!(profiles_from_local_state(r#"{"profile":{}}"#).is_none());
        assert!(profiles_from_local_state(r#"{"profile":{"info":{}}}"#).is_none());
    }

    #[test]
    fn list_profiles_reads_local_state_when_allowed() {
        let tmp = make_tmp_dir("ls-ok");
        std::fs::write(
            tmp.0.join("Local State"),
            r#"{"profile":{"last_used":"Profile 1","info":{"Default":{},"Profile 1":{}}}}"#,
        )
        .unwrap();
        let scan = list_profiles(&tmp.0);
        assert!(!scan.local_state_denied);
        assert_eq!(
            scan.profiles,
            vec!["Profile 1".to_string(), "Default".to_string()]
        );
    }

    /// Local State 被权限拒绝（用 mode 000 模拟 TCC 的 EACCES/EPERM）→
    /// 回退探测已知命名，只保留命中 Cookies 候选的 profile。
    /// root 进程不受 mode 位约束，此环境下跳过。
    #[test]
    fn list_profiles_denied_local_state_falls_back_to_probe() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = make_tmp_dir("ls-denied");
        let ls = tmp.0.join("Local State");
        std::fs::write(&ls, r#"{"profile":{"last_used":"Profile 1"}}"#).unwrap();
        std::fs::set_permissions(&ls, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_to_string(&ls).is_ok() {
            return; // 特权环境无法模拟权限拒绝
        }
        touch(&tmp.0.join("Default/Network/Cookies"));
        touch(&tmp.0.join("Profile 3/Cookies"));
        let scan = list_profiles(&tmp.0);
        assert!(scan.local_state_denied);
        assert_eq!(
            scan.profiles,
            vec!["Default".to_string(), "Profile 3".to_string()]
        );
    }

    // ---------- (host, name) 去重合并 ----------

    fn mk_cookie(host: &str, name: &str, value: &str) -> RawCookie {
        RawCookie {
            host_key: host.to_string(),
            name: name.to_string(),
            value: value.to_string(),
            path: "/".to_string(),
            expires_utc: 0,
            has_expires: false,
            is_secure: false,
            is_httponly: false,
            samesite: -1,
        }
    }

    #[test]
    fn push_dedup_keeps_first_by_host_and_name() {
        let mut cookies = Vec::new();
        let mut seen = HashSet::new();
        push_dedup(
            &mut cookies,
            &mut seen,
            mk_cookie(".kimi.com", "session", "first"),
        );
        // 跨 profile 的重复 (host, name)：丢弃后到者
        push_dedup(
            &mut cookies,
            &mut seen,
            mk_cookie(".kimi.com", "session", "second"),
        );
        // 同 host 不同名：保留
        push_dedup(&mut cookies, &mut seen, mk_cookie(".kimi.com", "pref", "zh"));
        // 同名不同 host：保留
        push_dedup(
            &mut cookies,
            &mut seen,
            mk_cookie("kimi.com", "session", "host-only"),
        );
        assert_eq!(cookies.len(), 3);
        assert_eq!(cookies[0].value, "first");
    }

    // ---------- scan_all_profiles ----------

    /// 在 path 建一个带 v10 加密行的真实 schema Cookies 库
    fn write_cookies_db(path: &Path, key: &[u8; 16], rows: &[(&str, &str, &str)]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(COOKIES_SCHEMA).unwrap();
        // 2099 年（chromium epoch 微秒）作为"未过期"
        let far_future: i64 = (4_102_444_800 + 11_644_473_600) * 1_000_000;
        for (host, name, value) in rows {
            conn.execute(
                "INSERT INTO cookies (creation_utc, host_key, name, value, encrypted_value, \
                 path, expires_utc, is_secure, is_httponly, has_expires, samesite) \
                 VALUES (0, ?1, ?2, '', ?3, '/', ?4, 1, 1, 1, 1)",
                rusqlite::params![host, name, encrypt_v10(key, value.as_bytes()), far_future],
            )
            .unwrap();
        }
    }

    fn scan_plan(profiles: &[&str], local_state_denied: bool) -> ProfileScan {
        ProfileScan {
            profiles: profiles.iter().map(|p| p.to_string()).collect(),
            local_state_denied,
        }
    }

    /// 两个 profile 各有 Cookies 库：逐一读取、计数求和、(host, name) 去重，
    /// 先扫描的 profile（Default）优先保留同名 cookie
    #[test]
    fn scan_all_profiles_merges_profiles_with_dedup() {
        let tmp = make_tmp_dir("multi");
        let key = *derive_cookie_key(b"peanuts");
        write_cookies_db(
            &tmp.0.join("Default/Cookies"),
            &key,
            &[
                (".kimi.com", "session", "tok-default"),
                (".kimi.com", "pref", "zh"),
            ],
        );
        write_cookies_db(
            &tmp.0.join("Profile 1/Cookies"),
            &key,
            &[
                (".kimi.com", "session", "tok-profile1"),
                (".chatgpt.com", "sess", "g"),
            ],
        );
        let hosts = vec!["www.kimi.com".to_string(), "chatgpt.com".to_string()];
        let out = scan_all_profiles(&tmp.0, &scan_plan(&["Default", "Profile 1"], false), "Edge", &key, &hosts)
            .unwrap();
        assert_eq!(
            out.profiles_scanned,
            vec!["Default".to_string(), "Profile 1".to_string()]
        );
        assert_eq!(out.matched, 4); // Default 2 条 + Profile 1 的 2 条（含跨 profile 重复）
        assert_eq!(out.cookies.len(), 3); // (host, name) 去重后
        let session = out
            .cookies
            .iter()
            .find(|c| c.host_key == ".kimi.com" && c.name == "session")
            .unwrap();
        assert_eq!(session.value, "tok-default");
    }

    /// 单个 profile 的 DB 损坏（查询失败）只跳过该 profile，不影响其余
    #[test]
    fn scan_all_profiles_isolates_per_profile_failure() {
        let tmp = make_tmp_dir("iso");
        let key = *derive_cookie_key(b"peanuts");
        // 垃圾字节：副本能打开但查询 cookies 表失败
        touch(&tmp.0.join("Default/Cookies"));
        write_cookies_db(
            &tmp.0.join("Profile 1/Cookies"),
            &key,
            &[(".kimi.com", "session", "tok")],
        );
        let out = scan_all_profiles(
            &tmp.0,
            &scan_plan(&["Default", "Profile 1"], false),
            "Edge",
            &key,
            &["www.kimi.com".to_string()],
        )
        .unwrap();
        assert_eq!(out.profiles_scanned.len(), 2); // 两个 DB 都命中并尝试
        assert_eq!(out.cookies.len(), 1);
        assert_eq!(out.cookies[0].value, "tok");
    }

    /// 全部 profile 读取失败 → 返回首个具体错误
    #[test]
    fn scan_all_profiles_all_failed_returns_first_error() {
        let tmp = make_tmp_dir("allfail");
        touch(&tmp.0.join("Default/Cookies"));
        touch(&tmp.0.join("Profile 1/Cookies"));
        let err = scan_all_profiles(
            &tmp.0,
            &scan_plan(&["Default", "Profile 1"], false),
            "Edge",
            &[0u8; 16],
            &[],
        )
        .unwrap_err();
        assert_eq!(err.code, E_COOKIE_DB_LOCKED);
    }

    /// Opera 式根目录布局：多个 profile 的候选解析到同一根目录 DB，只读一次
    #[test]
    fn scan_all_profiles_reads_shared_root_db_once() {
        let tmp = make_tmp_dir("opera-root");
        let key = *derive_cookie_key(b"peanuts");
        write_cookies_db(
            &tmp.0.join("Cookies"),
            &key,
            &[(".kimi.com", "session", "tok")],
        );
        let out = scan_all_profiles(
            &tmp.0,
            &scan_plan(&["Default", "Profile 1"], false),
            "Opera",
            &key,
            &["www.kimi.com".to_string()],
        )
        .unwrap();
        assert_eq!(out.profiles_scanned, vec!["Default".to_string()]);
        assert_eq!(out.matched, 1);
    }

    /// 复制 Cookies DB 被权限拒绝（mode 000 模拟 TCC）→ 立即整体失败，
    /// 且 Local State 同被拒时提示里合并该信号。root 环境跳过。
    #[test]
    fn scan_all_profiles_fails_fast_on_denied_copy() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = make_tmp_dir("tcc");
        let key = *derive_cookie_key(b"peanuts");
        let db = tmp.0.join("Default/Cookies");
        write_cookies_db(&db, &key, &[(".kimi.com", "session", "tok")]);
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&db).is_ok() {
            return; // 特权环境无法模拟权限拒绝
        }
        let err = scan_all_profiles(
            &tmp.0,
            &scan_plan(&["Default"], true),
            "Edge",
            &key,
            &["www.kimi.com".to_string()],
        )
        .unwrap_err();
        assert_eq!(err.code, E_BROWSER_DATA_TCC_DENIED);
        assert!(err.message.contains("Local State"));
    }

    /// 任何 DB 都没探测到：Local State 也被权限拒绝 → TCC（强信号）；
    /// 否则 → E_BROWSER_DATA_MISSING
    #[test]
    fn scan_all_profiles_no_db_reports_tcc_or_missing() {
        let tmp = make_tmp_dir("nodb");
        let err = scan_all_profiles(
            &tmp.0,
            &scan_plan(&["Default", "Profile 1"], true),
            "Edge",
            &[0u8; 16],
            &[],
        )
        .unwrap_err();
        assert_eq!(err.code, E_BROWSER_DATA_TCC_DENIED);

        let err = scan_all_profiles(
            &tmp.0,
            &scan_plan(&["Default", "Profile 1"], false),
            "Edge",
            &[0u8; 16],
            &[],
        )
        .unwrap_err();
        assert_eq!(err.code, E_BROWSER_DATA_MISSING);
    }

    // ---------- 启动自动导入守卫 ----------

    /// 测试间复位守卫（仅测试用；生产代码不 reset）
    #[cfg(test)]
    fn reset_auto_import_gate() {
        AUTO_IMPORT_ATTEMPTED.store(false, Ordering::SeqCst);
    }

    /// 首次 check-and-set 通过，第二次返回「已尝试」；复位后重新通过
    #[test]
    fn auto_import_gate_allows_once_per_process() {
        reset_auto_import_gate();
        assert!(!auto_import_gate()); // 首次：未尝试过，放行（并置位）
        assert!(auto_import_gate()); // 再次：已尝试过，拒绝
        assert!(auto_import_gate()); // 持续拒绝
        reset_auto_import_gate();
        assert!(!auto_import_gate()); // 复位后重新放行
        reset_auto_import_gate(); // 不污染其他测试
    }
}
