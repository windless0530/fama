//! 从默认 Chromium 系浏览器导入登录 cookies（用户显式触发，design.md §2.5）。
//!
//! 范围（有意收窄）：
//! - 只识别默认浏览器是否为 Chromium 系（Chrome / Edge / Brave / Vivaldi / Opera /
//!   Chromium）；Safari / Firefox / 未知直接报不支持，且不触发 Keychain 弹窗；
//! - 读取该浏览器的 `"<Browser> Safe Storage"` 钥匙串条目（macOS 会弹 ACL 授权框，
//!   为第二道用户授权），PBKDF2 派生 cookie 解密 key；
//! - 以临时副本只读查询浏览器 Cookies 数据库，只筛选已配置站点 host 匹配的条目；
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

use std::collections::HashMap;
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

/// `Local State` 里 `profile.last_used` 指示的目录名，回退 "Default"
fn last_used_profile(base: &Path) -> String {
    std::fs::read_to_string(base.join("Local State"))
        .ok()
        .and_then(|txt| serde_json::from_str::<serde_json::Value>(&txt).ok())
        .and_then(|v| {
            v.pointer("/profile/last_used")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "Default".to_string())
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
fn open_cookie_copy(src: &Path) -> Result<(rusqlite::Connection, TempDirGuard), ImportError> {
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
                return Err(ImportError::new(
                    E_COOKIE_DB_LOCKED,
                    format!("复制 Cookies 数据库失败: {e}"),
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
    /// 使用的浏览器 profile 目录名
    pub profile: String,
    /// host 匹配到的 cookie 行数
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

    // 3. 定位 Cookies 数据库
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
    let profile = last_used_profile(&base);
    let cookies_db = find_cookie_db(&base, &profile).ok_or_else(|| {
        ImportError::new(
            E_BROWSER_DATA_MISSING,
            format!(
                "未找到 {} 的 Cookies 数据库（已探测 profile 目录与数据根目录下的 \
                 Cookies / Network/Cookies；profile {} 可能尚未登录过任何站点）",
                browser.name, profile
            ),
        )
    })?;

    // 4. 副本查询 + host 过滤 + 解密
    let site_hosts: Vec<String> = targets
        .iter()
        .filter_map(|t| t.url.host_str().map(|h| h.to_string()))
        .collect();
    let (matched, skipped_decrypt, skipped_other, cookies) = {
        let (conn, _guard) = open_cookie_copy(&cookies_db)?;
        read_and_decrypt(&conn, &key, &site_hosts)?
    };

    // 5. 注入默认 data store（所有站点 webview 共享，见 design.md §2.5）
    let (injected, skipped_build) = inject_cookies(app, cookies)?;
    let skipped_other = skipped_other + skipped_build;

    if matched > 0 && injected == 0 && skipped_decrypt == matched {
        return Err(ImportError::new(
            E_DECRYPT_FAILED,
            format!(
                "匹配到 {matched} 条 cookie 但全部解密失败（浏览器可能启用了新的加密方案）"
            ),
        ));
    }

    // 6. reload 已加载的站点页面，让登录态立即生效
    let site_ids: Vec<String> = targets.iter().map(|t| t.id.clone()).collect();
    let reloaded = app.state::<Tabs>().reload_loaded(&site_ids);

    Ok(ImportReport {
        browser: browser.name.to_string(),
        profile,
        matched,
        injected,
        skipped_decrypt,
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

    /// 用 Chromium 真实 schema 造一个 Cookies 库，端到端验证 read_and_decrypt：
    /// host 过滤、v10 解密、明文 value 回退、过期剔除。
    #[test]
    fn read_and_decrypt_end_to_end() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE cookies (
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
            );",
        )
        .unwrap();

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
}
