//! 注入引擎：fill → verify → send → ack（design.md §2.3）
//!
//! 风控自律：本引擎只在用户主动触发 dispatch 时执行；
//! 无轮询、无后台任务；「仅填充」模式由 auto_send=false 表达。

use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};
use tauri_plugin_clipboard_manager::ClipboardExt;
use tokio::sync::Mutex as AsyncMutex;

use crate::sites::SiteConfig;
use crate::tabs::{self, EvalError, Tabs};

/// 全局 dispatch 互斥：同一时刻只允许一次发送（防连点重复注入）
static DISPATCH_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());

/// ACK nonce 单调递增，避免读到上一次的陈旧 ACK
static NONCE: AtomicU64 = AtomicU64::new(1);

/// 单站点注入结果（IPC 契约，camelCase）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DispatchResult {
    pub site_id: String,
    pub status: &'static str, // sent | filled | fallback | failed
    pub error: Option<String>,
}

/// 脚本 ACK 返回体
#[derive(Debug, Deserialize)]
struct Ack {
    stage: String,
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    held: bool,
    #[serde(default)]
    reason: Option<String>,
}

/// 组装自包含注入脚本（IIFE，不污染页面全局）
fn build_script(src: &str, question: &str, auto_send: bool, nonce: u64) -> String {
    let q = serde_json::to_string(question).unwrap_or_else(|_| "\"\"".to_string());
    let auto = if auto_send { "true" } else { "false" };
    format!(
        r#"(async () => {{
{src}
const __ack = (r) => {{
  try {{
    location.hash = "fama-ack={nonce}:" + encodeURIComponent(JSON.stringify(r));
  }} catch (e) {{ /* 页面禁止改 hash 时仅依赖返回值通道 */ }}
  return r;
}};
try {{
  const __f = await famaFill({q});
  if (!__f || !__f.ok) return __ack({{ stage: "fill", ok: false, reason: (__f && __f.reason) || "fill-failed" }});
  if (!{auto}) return __ack({{ stage: "fill", ok: true, held: true }});
  const __s = await famaSend();
  return __ack({{ stage: "send", ok: !!(__s && __s.ok), reason: (__s && __s.reason) || null }});
}} catch (e) {{
  return __ack({{ stage: "error", ok: false, reason: String(e) }});
}}
}})()"#
    )
}

/// 对一个站点执行一次注入尝试
async fn inject_once(
    app: &AppHandle<tauri::Wry>,
    tabs: &Tabs,
    site: &SiteConfig,
    question: &str,
    auto_send: bool,
    nonce: u64,
) -> Result<Ack, EvalError> {
    let js = build_script(site.injector_src, question, auto_send, nonce);
    let v = tabs::eval_with_ack(app, tabs, &site.id, &js, nonce).await?;
    serde_json::from_value(v).map_err(EvalError::from)
}

/// 单站点完整流程：fill（失败重试 1 次）→ 可选 send → 降级剪贴板
async fn inject_one(
    app: &AppHandle<tauri::Wry>,
    tabs: &Tabs,
    site: &SiteConfig,
    question: &str,
    auto_send: bool,
) -> DispatchResult {
    if let Err(e) = tabs::ensure_tab(app, tabs, site) {
        return DispatchResult {
            site_id: site.id.clone(),
            status: "failed",
            error: Some(e),
        };
    }

    let mut last_err = String::new();
    for attempt in 0..2 {
        let nonce = NONCE.fetch_add(1, Ordering::Relaxed) + attempt;
        match inject_once(app, tabs, site, question, auto_send, nonce).await {
            Ok(ack) => {
                if ack.ok {
                    let status = if auto_send && !ack.held {
                        "sent"
                    } else {
                        "filled"
                    };
                    return DispatchResult {
                        site_id: site.id.clone(),
                        status,
                        error: None,
                    };
                }
                last_err = format!("{}: {}", ack.stage, ack.reason.unwrap_or_default());
            }
            Err(e) => last_err = e.to_string(),
        }
        if attempt == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await; // 页面可能未就绪
        }
    }

    // 降级：问题进剪贴板，用户切到该 tab 一键粘贴
    let clip_err = app
        .clipboard()
        .write_text(question)
        .err()
        .map(|e| e.to_string());
    DispatchResult {
        site_id: site.id.clone(),
        status: if clip_err.is_none() {
            "fallback"
        } else {
            "failed"
        },
        error: Some(if let Some(ce) = clip_err {
            format!("{last_err}; 剪贴板降级也失败: {ce}")
        } else {
            last_err
        }),
    }
}

/// 并行分发到多个站点。返回顺序与 sites 一致。
pub async fn dispatch(
    app: &AppHandle<tauri::Wry>,
    targets: &[SiteConfig],
    question: &str,
    auto_send: bool,
) -> Vec<DispatchResult> {
    let _guard = DISPATCH_LOCK.lock().await;

    let mut handles = Vec::with_capacity(targets.len());
    for site in targets {
        let app = app.clone();
        let site = site.clone();
        let q = question.to_string();
        handles.push(tauri::async_runtime::spawn(async move {
            // 任务内取托管状态（spawn 需要 'static，不能借用 &Tabs）
            let tabs = app.state::<Tabs>();
            inject_one(&app, &tabs, &site, &q, auto_send).await
        }));
    }

    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        match h.await {
            Ok(r) => out.push(r),
            Err(e) => out.push(DispatchResult {
                site_id: "?".to_string(),
                status: "failed",
                error: Some(format!("任务异常: {e}")),
            }),
        }
    }
    out
}
