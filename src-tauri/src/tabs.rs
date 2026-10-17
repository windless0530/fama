//! 站点 webview 管理（design.md §2.2）。
//!
//! M0 实现 desktop 路径：Tauri v2 子 webview（wry）。
//! iOS 路线（Tauri 多 webview 或 FamaTabs Swift 插件）在 M0 spike 后落进本模块，
//! 上层接口保持不变。
//!
//! webview 句柄由本模块自管注册表持有（创建后插入），不依赖运行时枚举 API。

use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{
    AppHandle, LogicalPosition, LogicalSize, Manager, Webview, WebviewBuilder, WebviewUrl, Wry,
};

use crate::sites::SiteConfig;

const CREATE_TIMEOUT: Duration = Duration::from_secs(10);

/// 前端上报的布局项（IPC 契约，camelCase）
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LayoutItem {
    pub site_id: String,
    pub rect: Rect,
}

/// 站点 webview 注册表：句柄 + 最近一次 rect
pub struct Tabs {
    webviews: Mutex<HashMap<String, Webview<Wry>>>,
    rects: Mutex<HashMap<String, Rect>>,
}

impl Tabs {
    pub fn new() -> Self {
        Self {
            webviews: Mutex::new(HashMap::new()),
            rects: Mutex::new(HashMap::new()),
        }
    }

    fn get(&self, site_id: &str) -> Option<Webview<Wry>> {
        self.webviews.lock().unwrap().get(site_id).cloned()
    }
}

/// 幂等创建站点 webview。创建动作派发到主线程执行，句柄回传存入注册表。
pub fn ensure_tab(app: &AppHandle<Wry>, tabs: &Tabs, site: &SiteConfig) -> Result<(), String> {
    if tabs.get(&site.id).is_some() {
        return Ok(());
    }
    let (tx, rx) = mpsc::channel();
    let site_id = site.id.clone();
    let site = site.clone();
    let app2 = app.clone();
    app.run_on_main_thread(move || {
        let res: Result<Webview<Wry>, String> = app2
            .get_webview_window("main")
            .ok_or_else(|| "main window 不存在".to_string())
            .and_then(|_| {
                let window = app2
                    .get_window("main")
                    .ok_or_else(|| "main window 不存在".to_string())?;
                let wv = window
                    .add_child(
                        WebviewBuilder::new(
                            site.id.clone(),
                            WebviewUrl::External(site.url.clone()),
                        ),
                        LogicalPosition::new(0.0, 0.0),
                        LogicalSize::new(1.0, 1.0),
                    )
                    .map_err(|e| e.to_string())?;
                // 先隐藏，等 layout_update 给出真实位置再显示
                wv.hide().map_err(|e| e.to_string())?;
                Ok(wv)
            });
        let _ = tx.send(res);
    })
    .map_err(|e| format!("派发主线程失败: {e}"))?;

    match rx.recv_timeout(CREATE_TIMEOUT) {
        Ok(res) => {
            let wv = res?;
            tabs.webviews.lock().unwrap().insert(site_id, wv);
            Ok(())
        }
        Err(_) => Err("webview 创建超时".to_string()),
    }
}

/// 应用布局：列出的站点按 rect 摆放并显示，未列出的隐藏。
/// 单 tab = 1 项；分栏 = N 项（M3）。
/// 调用方（commands 层）需先对列表内站点 ensure_tab。
pub fn apply_layout(app: &AppHandle<Wry>, tabs: &Tabs, items: &[LayoutItem]) -> Result<(), String> {
    {
        let mut m = tabs.rects.lock().unwrap();
        for it in items {
            m.insert(it.site_id.clone(), it.rect.clone());
        }
    }

    let handles: Vec<(String, Webview<Wry>)> = tabs
        .webviews
        .lock()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let items: Vec<(String, Rect)> = items
        .iter()
        .map(|i| (i.site_id.clone(), i.rect.clone()))
        .collect();

    let (tx, rx) = mpsc::channel();
    app.run_on_main_thread(move || {
        let mut res = Ok(());
        // 遍历注册表中的站点 webview：在列表中的摆放+显示，不在的隐藏
        for (label, wv) in handles {
            if let Some((_, rect)) = items.iter().find(|(id, _)| *id == label) {
                if let Err(e) = wv
                    .set_position(LogicalPosition::new(rect.x, rect.y))
                    .and_then(|_| wv.set_size(LogicalSize::new(rect.w, rect.h)))
                    .and_then(|_| wv.show())
                {
                    res = Err(format!("布局 {label} 失败: {e}"));
                }
            } else if let Err(e) = wv.hide() {
                res = Err(format!("隐藏 {label} 失败: {e}"));
            }
        }
        let _ = tx.send(res);
    })
    .map_err(|e| format!("派发主线程失败: {e}"))?;

    match rx.recv_timeout(CREATE_TIMEOUT) {
        Ok(res) => res,
        Err(_) => Err("布局应用超时".to_string()),
    }
}

/// 激活单个站点（用最近一次 rect；没有则报错——前端应先 layout_update）
pub fn activate(app: &AppHandle<Wry>, tabs: &Tabs, site_id: &str) -> Result<(), String> {
    let rect = tabs
        .rects
        .lock()
        .unwrap()
        .get(site_id)
        .cloned()
        .ok_or_else(|| format!("站点 {site_id} 尚无布局信息"))?;
    apply_layout(
        app,
        tabs,
        &[LayoutItem {
            site_id: site_id.to_string(),
            rect,
        }],
    )
}

/// 注入错误（inject.rs 消费）
#[derive(Debug)]
pub enum EvalError {
    TabNotReady,
    Script(String),
    Timeout,
    Decode(String),
}

impl std::fmt::Display for EvalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EvalError::TabNotReady => write!(f, "webview 未就绪"),
            EvalError::Script(s) => write!(f, "脚本执行失败: {s}"),
            EvalError::Timeout => write!(f, "结果回传超时（hash-ACK 未出现）"),
            EvalError::Decode(s) => write!(f, "ACK 解析失败: {s}"),
        }
    }
}

impl From<serde_json::Error> for EvalError {
    fn from(e: serde_json::Error) -> Self {
        EvalError::Decode(e.to_string())
    }
}

const ACK_TIMEOUT: Duration = Duration::from_millis(3000);
const ACK_POLL_INTERVAL: Duration = Duration::from_millis(150);

/// 在站点页面执行脚本并等待 hash-ACK 结果（design.md §2.3 双通道的兜底通道；
/// iOS 原生 eval 带返回值后，本函数退化为直接解析返回值）。
pub async fn eval_with_ack(
    app: &AppHandle<Wry>,
    tabs: &Tabs,
    site_id: &str,
    js: &str,
    nonce: u64,
) -> Result<serde_json::Value, EvalError> {
    let wv = tabs.get(site_id).ok_or(EvalError::TabNotReady)?;
    wv.eval(js).map_err(|e| EvalError::Script(e.to_string()))?;

    let prefix = format!("fama-ack={nonce}:");
    let deadline = Instant::now() + ACK_TIMEOUT;
    loop {
        let frag = read_fragment(app, tabs, site_id);
        if let Some(f) = frag {
            if let Some(rest) = f.strip_prefix(&prefix) {
                let decoded = percent_encoding::percent_decode_str(rest)
                    .decode_utf8()
                    .map_err(|e| EvalError::Decode(e.to_string()))?;
                return Ok(serde_json::from_str(&decoded)?);
            }
        }
        if Instant::now() >= deadline {
            return Err(EvalError::Timeout);
        }
        tokio::time::sleep(ACK_POLL_INTERVAL).await;
    }
}

/// 读取站点 webview 当前 URL 的 fragment（读取派发到主线程，同步等待）
fn read_fragment(app: &AppHandle<Wry>, tabs: &Tabs, site_id: &str) -> Option<String> {
    let wv = tabs.get(site_id)?;
    let (tx, rx) = mpsc::channel();
    app.run_on_main_thread(move || {
        // 站点页 URL 形如 https://host/path#fama-ack=...；SPA 改 hash 不触发导航
        let _ = tx.send(
            wv.url()
                .ok()
                .and_then(|u| u.fragment().map(|s| s.to_string())),
        );
    })
    .ok()?;
    rx.recv_timeout(Duration::from_millis(500)).ok().flatten()
}
