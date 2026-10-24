//! 非 macOS 平台的浏览器登录态导入 stub（与 browser_session.rs 同签名）。
//!
//! Windows 的等价能力是 DPAPI + Cookies DB、Linux 是 Secret Service，
//! 均不在本次范围内（见 plan 与 design.md §2.5），先返回不支持。

use serde::Serialize;
use tauri::{AppHandle, Wry};

use crate::sites::SiteConfig;

/// 导入报告（与 macOS 实现的字段保持一致；本平台不会构造）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct ImportReport {
    pub browser: String,
    pub profile: String,
    pub profiles_scanned: Vec<String>,
    pub matched: usize,
    pub injected: usize,
    pub skipped_decrypt: usize,
    pub skipped_other: usize,
    pub reloaded: Vec<String>,
}

/// 带 "E_XXX" 错误码的导入失败（与 macOS 实现同形，commands 层统一拼接）
#[derive(Debug)]
#[allow(dead_code)]
pub struct ImportError {
    pub code: &'static str,
    pub message: String,
}

pub fn import(_app: &AppHandle<Wry>, _targets: &[SiteConfig]) -> Result<ImportReport, ImportError> {
    Err(ImportError {
        code: "E_UNSUPPORTED_PLATFORM",
        message: "浏览器登录态导入目前仅支持 macOS".into(),
    })
}
