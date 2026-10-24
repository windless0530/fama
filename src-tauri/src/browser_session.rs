//! 从默认 Chromium 系浏览器导入登录 cookies（用户显式触发，design.md §2.5）。
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
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Manager, Wry};
use zeroize::Zeroizing;

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
            return Ok(k.clone());
        }
    }
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

/// 查询 + host 过滤 + 解密。返回 (matched, skipped_decrypt, skipped_other, cookies)
#[allow(clippy::type_complexity)]
fn read_and_decrypt(
    conn: &rusqlite::Connection,
    key: &[u8; 16],
    site_hosts: &[String],
) -> Result<(usize, usize, usize, Vec<RawCookie>), ImportError> {
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

    let mut rows = stmt
        .query([])
        .map_err(|e| ImportError::new(E_COOKIE_DB_LOCKED, format!("遍历 cookies 表失败: {e}")))?;

    let (mut matched, mut skipped_decrypt, mut skipped_other) = (0, 0, 0);
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
            Err(_) => {
                skipped_other += 1;
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
        let expires_utc: i64 = row.get(5).unwrap_or(0);
        let is_secure: bool = row.get::<_, i64>(6).map(|v| v != 0).unwrap_or(false);
        let is_httponly: bool = row.get::<_, i64>(7).map(|v| v != 0).unwrap_or(false);
        let has_expires: bool = row.get::<_, i64>(8).map(|v| v != 0).unwrap_or(false);
        let samesite: i64 = row.get(9).unwrap_or(-1);

        // 过期 cookie 不导入
        if has_expires && chromium_utc_to_unix_secs(expires_utc) <= now_unix {
            skipped_other += 1;
            continue;
        }
        let plain: Vec<u8> = if encrypted.is_empty() {
            value.into_bytes()
        } else {
            match decrypt_v10(key, &encrypted) {
                Ok(p) => p,
                Err(_) => {
                    skipped_decrypt += 1;
                    continue;
                }
            }
        };
        let value_str = match String::from_utf8(plain) {
            Ok(s) => s,
            Err(_) => {
                skipped_other += 1;
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
    Ok((matched, skipped_decrypt, skipped_other, cookies))
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
    skipped_decrypt: usize,
    skipped_other: usize,
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
        skipped_decrypt: 0,
        skipped_other: 0,
        cookies: Vec::new(),
    };
    let mut seen_dbs: HashSet<PathBuf> = HashSet::new();
    let mut seen_keys: HashSet<(String, String)> = HashSet::new();
    let mut read_ok_any = false;
    let mut first_err: Option<ImportError> = None;

    for profile in &scan.profiles {
        let Some(db) = find_cookie_db(base, profile) else { continue };
        if !seen_dbs.insert(db.clone()) {
            continue;
        }
        out.profiles_scanned.push(profile.clone());
        let (conn, _guard) = match open_cookie_copy(&db, browser_name, scan.local_state_denied) {
            Ok(v) => v,
            // TCC：一次失败即整体返回
            Err(e) if e.code == E_BROWSER_DATA_TCC_DENIED => return Err(e),
            // 单 profile 失败不影响其他 profile
            Err(e) => {
                first_err.get_or_insert(e);
                continue;
            }
        };
        match read_and_decrypt(&conn, key, site_hosts) {
            Ok((matched, skipped_decrypt, skipped_other, cookies)) => {
                read_ok_any = true;
                out.matched += matched;
                out.skipped_decrypt += skipped_decrypt;
                out.skipped_other += skipped_other;
                for c in cookies {
                    push_dedup(&mut out.cookies, &mut seen_keys, c);
                }
            }
            Err(e) => {
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
                ns_string!("Version"),
            ],
            &[
                &*name as &AnyObject,
                &*value as &AnyObject,
                &*path as &AnyObject,
                &*NSString::from_str("0") as &AnyObject,
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
}

/// 完整导入流程。调用前提：用户已在前端确认框同意（第一道授权）；
/// Keychain ACL 弹窗（第二道授权）由系统在本函数读 Safe Storage 时弹出。
pub fn import(app: &AppHandle<Wry>, targets: &[SiteConfig]) -> Result<ImportReport, ImportError> {
    if targets.is_empty() {
        return Err(ImportError::new(E_NO_SITES, "未选择任何有效站点"));
    }

    // 1. 识别默认浏览器（不支持的浏览器在这里就返回，不触发 Keychain）
    let bundle_id = detect_default_browser_bundle_id(app)?;
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

    // 3. 定位用户数据目录（TCC 下元数据访问仍可用，is_dir 不会被拦截）
    let base = app
        .path()
        .home_dir()
        .map_err(|e| ImportError::new(E_BROWSER_DATA_MISSING, format!("取 home 目录失败: {e}")))?
        .join("Library/Application Support")
        .join(browser.data_dir);
    if !base.is_dir() {
        return Err(ImportError::new(
            E_BROWSER_DATA_MISSING,
            format!("未找到 {} 的用户数据目录 {}", browser.name, base.display()),
        ));
    }

    // 4. 多 profile 枚举 + 逐一扫描（副本查询 + host 过滤 + 解密 + 去重合并）。
    //    TCC 权限拒绝（Local State / Cookies 复制 EPERM）在这里以
    //    E_BROWSER_DATA_TCC_DENIED 一次失败即返回。
    let site_hosts: Vec<String> = targets
        .iter()
        .filter_map(|t| t.url.host_str().map(|h| h.to_string()))
        .collect();
    let scan = list_profiles(&base);
    let outcome = scan_all_profiles(&base, &scan, browser.name, &key, &site_hosts)?;

    // 5. 注入默认 data store（所有站点 webview 共享，见 design.md §2.5）
    let (injected, skipped_build) = inject_cookies(app, outcome.cookies)?;
    let skipped_other = outcome.skipped_other + skipped_build;

    if outcome.matched > 0 && injected == 0 && outcome.skipped_decrypt == outcome.matched {
        return Err(ImportError::new(
            E_DECRYPT_FAILED,
            format!(
                "匹配到 {} 条 cookie 但全部解密失败（浏览器可能启用了新的加密方案）",
                outcome.matched
            ),
        ));
    }

    // 6. reload 已加载的站点页面，让登录态立即生效
    let site_ids: Vec<String> = targets.iter().map(|t| t.id.clone()).collect();
    let reloaded = app.state::<Tabs>().reload_loaded(&site_ids);

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
        skipped_decrypt: outcome.skipped_decrypt,
        skipped_other,
        reloaded,
    })
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
        let (matched, skipped_decrypt, skipped_other, cookies) =
            read_and_decrypt(&conn, &key, &hosts).unwrap();

        assert_eq!(matched, 4); // .kimi.com 的 4 条（不含 example.com）
        assert_eq!(skipped_decrypt, 1); // v20
        assert_eq!(skipped_other, 1); // 过期
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
}
