//! 站点 webview 管理（design.md §2.2）。
//!
//! M0 实现 desktop 路径：Tauri v2 子 webview（wry）。
//! iOS 路线（Tauri 多 webview 或 FamaTabs Swift 插件）在 M0 spike 后落进本模块，
//! 上层接口保持不变。
//!
//! webview 句柄由本模块自管注册表持有（创建后插入），不依赖运行时枚举 API。

use std::collections::{HashMap, HashSet};
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::webview::PageLoadEvent;
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
    /// 已完成首次页面加载的站点。
    /// macOS 上 wry 的 url() 对未加载的 WKWebView 会 panic（URL 为 nil 时 unwrap），
    /// 读取 URL / eval 前必须先确认页面就绪。
    loaded: Mutex<HashSet<String>>,
    /// 当前处于"可见"状态的站点（apply_layout 显示后插入、隐藏时移除）。
    /// 从隐藏恢复显示时 WKWebView 的远端图层可能停留在旧几何（隐藏期间的
    /// frame 变化不进图层），需要跨 CA 事务的分步重设，见 apply_layout。
    shown: Mutex<HashSet<String>>,
    /// cookie 导入注入时仍未完成首次加载的站点：其导航请求早于注入发出、
    /// 不带新 cookie，首次加载完成（on_page_load Finished）时补一次 reload。
    /// consume 与 insert 都以 pending_reload 锁串行化，reload 只触发一次。
    pending_reload: Mutex<HashSet<String>>,
    /// localStorage 导入（kimi 等不走 cookie 认证的站点）：site_id → 注入
    /// 脚本（localStorage.setItem 批量）。注入必须在对应 origin 的页面
    /// 上下文执行——未完成首载的站点挂起，首载 Finished 时 eval + reload
    /// （remove 语义消费，防循环）；该 reload 同时让已注入的 cookie 生效，
    /// 与 pending_reload 互斥（见 reload_loaded）。
    pending_ls: Mutex<HashMap<String, String>>,
    /// 创建互斥：preload / layout_update / dispatch 可能并发 ensure_tab，
    /// 注册表插入在创建之后，需要串行化 + 双重检查防止重复创建。
    creating: Mutex<()>,
}

impl Tabs {
    pub fn new() -> Self {
        Self {
            webviews: Mutex::new(HashMap::new()),
            rects: Mutex::new(HashMap::new()),
            loaded: Mutex::new(HashSet::new()),
            shown: Mutex::new(HashSet::new()),
            pending_reload: Mutex::new(HashSet::new()),
            pending_ls: Mutex::new(HashMap::new()),
            creating: Mutex::new(()),
        }
    }

    fn get(&self, site_id: &str) -> Option<Webview<Wry>> {
        self.webviews.lock().unwrap().get(site_id).cloned()
    }

    fn mark_loaded(&self, site_id: &str) {
        self.loaded.lock().unwrap().insert(site_id.to_string());
    }

    fn is_loaded(&self, site_id: &str) -> bool {
        self.loaded.lock().unwrap().contains(site_id)
    }

    /// reload 已完成首次加载的站点 webview（浏览器登录态导入后刷新用）。
    /// - 已加载的站点：有 localStorage 注入脚本的先 eval `localStorage.setItem`
    ///   再 reload（一次 reload 同时让 cookie 与 localStorage 生效）；否则仅 reload；
    /// - 尚未完成首载的站点：有注入脚本的转入 pending_ls（首载 Finished 时
    ///   eval + reload），否则转入 pending_reload（首载 Finished 时补 reload）；
    /// - 尚未创建的站点无需处理（创建时数据已在，首导航即带上）。
    ///
    /// `ls_scripts` 为空时行为与纯 cookie 导入完全一致。
    pub fn reload_loaded(&self, site_ids: &[String], ls_scripts: &HashMap<String, String>) -> ImportReload {
        let mut out = ImportReload::default();
        // 锁序：loaded → webviews → pending_reload → pending_ls（consume_* 只单独
        // 取各自锁，无反向嵌套，无死锁）
        let loaded = self.loaded.lock().unwrap();
        let webviews = self.webviews.lock().unwrap();
        let mut pending = self.pending_reload.lock().unwrap();
        let mut pending_ls = self.pending_ls.lock().unwrap();
        for (id, wv) in webviews.iter() {
            if !site_ids.contains(id) {
                continue;
            }
            let script = ls_scripts.get(id);
            if loaded.contains(id) {
                let mut ok = true;
                if let Some(js) = script {
                    // WKWebView 的 localStorage 按导航 origin 隔离；此时页面已
                    // 在站点 origin 上，eval 直接写入该 origin 的 localStorage
                    ok = wv.eval(js).is_ok();
                    if ok {
                        out.ls_injected_now += ls_scripts_entry_count(js);
                    }
                }
                if ok && wv.eval("location.reload()").is_ok() {
                    out.reloaded.push(id.clone());
                }
            } else if let Some(js) = script {
                out.ls_pending += ls_scripts_entry_count(js);
                pending_ls.insert(id.clone(), js.clone());
            } else {
                pending.insert(id.clone());
            }
        }
        out
    }

    /// 首次加载完成时消费 pending_reload 标记：存在则返回 true（调用方应 reload）。
    /// remove 语义保证同一站点一次导入只补一次 reload，不会循环。
    fn consume_pending_reload(&self, site_id: &str) -> bool {
        self.pending_reload.lock().unwrap().remove(site_id)
    }

    /// 首次加载完成时消费 pending_ls：存在则返回注入脚本（调用方 eval 后 reload）。
    fn consume_pending_ls(&self, site_id: &str) -> Option<String> {
        self.pending_ls.lock().unwrap().remove(site_id)
    }
}

/// reload_loaded 的结果（browser_session 拼进 ImportReport）
#[derive(Debug, Default)]
pub struct ImportReload {
    /// 实际立即触发 reload 的站点
    pub reloaded: Vec<String>,
    /// 已加载站点上立即 eval 注入的 localStorage 条数
    pub ls_injected_now: usize,
    /// 挂起待首载完成注入的 localStorage 条数
    pub ls_pending: usize,
}

/// 从注入脚本里数出条目数（脚本由 browser_session::build_ls_inject_js 生成，
/// 首行注释带 `// entries: N`——避免 eval 结果无法同步回读的计数手段）
fn ls_scripts_entry_count(js: &str) -> usize {
    js.lines()
        .next()
        .and_then(|l| l.trim().strip_prefix("// entries: "))
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(0)
}

/// 幂等创建站点 webview。创建动作派发到主线程执行，句柄回传存入注册表。
pub fn ensure_tab(app: &AppHandle<Wry>, tabs: &Tabs, site: &SiteConfig) -> Result<(), String> {
    if tabs.get(&site.id).is_some() {
        return Ok(());
    }
    // 串行化创建（持锁跨越主线程往返；主线程本身不取此锁，无死锁）
    let _guard = tabs.creating.lock().unwrap();
    // 双重检查：等锁期间可能已被并发调用创建
    if tabs.get(&site.id).is_some() {
        return Ok(());
    }
    let (tx, rx) = mpsc::channel();
    let site_id = site.id.clone();
    let site = site.clone();
    let app2 = app.clone();
    eprintln!("[tabs] create {} {}", site.id, site.url);
    app.run_on_main_thread(move || {
        let res: Result<Webview<Wry>, String> = (|| {
            // 配置文件创建的窗口只注册在 webview 表里（get_window 查不到），
            // 经 UI webview 反查宿主 Window 才能挂子 webview
            let window = app2
                .get_webview("main")
                .ok_or_else(|| "main UI webview 不存在".to_string())?
                .window();
            // 初建 frame 必须给"标题栏以下的正常尺寸"：
            // macOS 上 WKWebView 以创建时 frame 减窗口安全区（标题栏 32pt）确定首个布局视口；
            // 1×1 隐藏在 (0,0)（fullSizeContentView 下正处于标题栏底下）会把视口钳成 1×(1-32) 的负高度，
            // 页面后续一直按错误视口排版，渲染出上下内缩、透出占位网格的现象。
            let (init_w, init_h) = {
                let scale = window.scale_factor().unwrap_or(1.0);
                window
                    .inner_size()
                    .map(|s| s.to_logical::<f64>(scale))
                    .map(|s| (s.width.max(100.0), (s.height - 44.0).max(100.0)))
                    .unwrap_or((1200.0, 756.0))
            };
            let wv = match window.add_child(
                WebviewBuilder::new(
                    site.id.clone(),
                    WebviewUrl::External(site.url.clone()),
                )
                .on_navigation(|url| {
                    eprintln!("[tabs] navigation {url}");
                    true
                })
                .on_page_load(|wv, payload| {
                    eprintln!("[tabs] {} page {:?} {}", wv.label(), payload.event(), payload.url());
                    if matches!(payload.event(), PageLoadEvent::Finished) {
                        let tabs = wv.state::<Tabs>();
                        tabs.mark_loaded(wv.label());
                        // 导入后才完成首载的页面：导航请求早于注入发出。
                        // localStorage 注入优先（eval 写入后 reload，一次刷新同时
                        // 生效 cookie 与 localStorage，并把同站点的 pending_reload
                        // 一并消费掉防二次 reload）；否则按 cookie 路径补 reload。
                        // consume 的 remove 语义防循环。
                        if let Some(js) = tabs.consume_pending_ls(wv.label()) {
                            eprintln!("[tabs] {} post-import localStorage inject + reload", wv.label());
                            let _ = wv.eval(&js);
                            let _ = wv.eval("location.reload()");
                            let _ = tabs.consume_pending_reload(wv.label());
                        } else if tabs.consume_pending_reload(wv.label()) {
                            eprintln!("[tabs] {} post-import reload", wv.label());
                            let _ = wv.eval("location.reload()");
                        }
                    }
                }),
                LogicalPosition::new(0.0, 44.0),
                LogicalSize::new(init_w, init_h),
            ) {
                Ok(wv) => wv,
                // 已存在（异常时序残留）：改为接管现有句柄，保持幂等
                Err(tauri::Error::WebviewLabelAlreadyExists(_)) => app2
                    .get_webview(&site.id)
                    .ok_or_else(|| "webview 已存在但获取句柄失败".to_string())?,
                Err(e) => return Err(e.to_string()),
            };
            // 先隐藏，等 layout_update 给出真实位置再显示。
            // frame 同时缩为 0：WKWebView 的远端图层在页面首次合成时可能
            // 无视 hidden 标志继续绘制（幽灵页），0 尺寸图层无处可画。
            // 显示路径（apply_layout）会先设目标 frame 再 show+nudge，视口
            // 以 nudge 强制重同步，不受此处 0 尺寸影响。
            wv.hide().map_err(|e| e.to_string())?;
            wv.set_bounds(tauri::Rect {
                position: LogicalPosition::new(0.0, 0.0).into(),
                size: LogicalSize::new(0.0, 0.0).into(),
            })
            .map_err(|e| e.to_string())?;
            Ok(wv)
        })();
        let _ = tx.send(res);
    })
    .map_err(|e| format!("派发主线程失败: {e}"))?;

    match rx.recv_timeout(CREATE_TIMEOUT) {
        Ok(res) => {
            let wv = match res {
                Ok(wv) => wv,
                Err(e) => {
                    eprintln!("[tabs] {} create failed: {}", site_id, e);
                    return Err(e);
                }
            };
            eprintln!("[tabs] {} registered", site_id);
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
    eprintln!("[tabs] apply layout: {:?}", items);

    // 哪些站点需要"从隐藏恢复"的完整重同步序列
    let mut first_show: Vec<String> = Vec::new();
    {
        let mut shown = tabs.shown.lock().unwrap();
        for it in &items {
            if !shown.contains(&it.0) {
                first_show.push(it.0.clone());
            }
        }
        shown.retain(|id| items.iter().any(|(sid, _)| sid == id));
        for it in &items {
            shown.insert(it.0.clone());
        }
    }

    let (tx, rx) = mpsc::channel();
    let app2 = app.clone();
    app.run_on_main_thread(move || {
        let mut res = Ok(());
        // 遍历注册表中的站点 webview：在列表中的摆放+显示，不在的隐藏
        for (label, wv) in handles {
            if let Some((_, rect)) = items.iter().find(|(id, _)| *id == label) {
                let bounds = tauri::Rect {
                    position: LogicalPosition::new(rect.x, rect.y).into(),
                    size: LogicalSize::new(rect.w, rect.h).into(),
                };
                // 位置和尺寸必须一次设置：macOS 上分两次调用会读取旧 frame，
                // 在子 webview 上可能产生错误的纵向偏移或上下留白。
                let is_first_show = first_show.iter().any(|id| id == &label);
                if is_first_show {
                    // 从隐藏恢复：WKWebView 的远端图层可能停留在旧几何（隐藏期间
                    // 的 frame 变化不进图层，且同一主线程回合内的多次 setFrame 会
                    // 合并成一个 CA 事务、净零变化被直接去重）。
                    // 必须跨事务分步提交，保证"可见状态下存在真实的相邻事务差异"：
                    // show 与两次 frame 变化各自落在不同 runloop 回合（各自提交）。
                    let app3 = app2.clone();
                    let (tx_, ty_, tw_, th_) = (rect.x, rect.y, rect.w, rect.h);
                    let _ = wv.show();
                    let wv1 = wv.clone();
                    let wv2 = wv.clone();
                    let wv3 = wv.clone();
                    std::thread::spawn(move || {
                        let mk = move |h: f64| tauri::Rect {
                            position: LogicalPosition::new(tx_, ty_).into(),
                            size: LogicalSize::new(tw_, h).into(),
                        };
                        std::thread::sleep(Duration::from_millis(50));
                        let _ = app3.run_on_main_thread(move || {
                            let _ = wv1.set_bounds(mk(th_));
                        });
                        std::thread::sleep(Duration::from_millis(50));
                        let _ = app3.run_on_main_thread(move || {
                            let _ = wv2.set_bounds(mk((th_ - 1.0).max(1.0)));
                        });
                        std::thread::sleep(Duration::from_millis(50));
                        let _ = app3.run_on_main_thread(move || {
                            let _ = wv3.set_bounds(mk(th_));
                        });
                    });
                } else if let Err(e) = wv.set_bounds(bounds) {
                    // 可见状态的常规重排：单次真实 frame 变化本事务内即生效
                    res = Err(format!("布局 {label} 失败: {e}"));
                }
            } else {
                // 隐藏时必须同时把 frame 缩为 0：WKWebView 的远端图层合成
                // 不总是尊重 NSView hidden（hide 落在页面首次合成附近时，
                // 内容进程仍会把图层绘制到屏幕上，形成盖住一切的"幽灵页"）。
                // 0 尺寸的图层无处可画，从机制上杜绝该现象。
                let zero = tauri::Rect {
                    position: LogicalPosition::new(0.0, 0.0).into(),
                    size: LogicalSize::new(0.0, 0.0).into(),
                };
                if let Err(e) = wv.set_bounds(zero).and_then(|_| wv.hide()) {
                    res = Err(format!("隐藏 {label} 失败: {e}"));
                }
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
/// 等待页面完成首次加载的上限（AI 站点首屏较重）
const READY_TIMEOUT: Duration = Duration::from_secs(8);

/// 等待站点页面完成首次加载：页面未就绪时 wry 的 url() 会 panic、eval 也无处执行
async fn wait_page_ready(tabs: &Tabs, site_id: &str) -> Result<(), EvalError> {
    let deadline = Instant::now() + READY_TIMEOUT;
    while !tabs.is_loaded(site_id) {
        if Instant::now() >= deadline {
            return Err(EvalError::TabNotReady);
        }
        tokio::time::sleep(ACK_POLL_INTERVAL).await;
    }
    Ok(())
}

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
    wait_page_ready(tabs, site_id).await?;
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
    tabs.url_fragment(app, site_id)
}

impl Tabs {
    /// eval 诊断探针到已完成首载的站点页面（browser_session 的 FAMA_DEBUG_IMPORT
    /// 回读验证用）；页面未就绪 / eval 失败返回 false
    pub fn eval_probe_if_loaded(&self, site_id: &str, js: &str) -> bool {
        match self.get(site_id) {
            Some(wv) if self.is_loaded(site_id) => wv.eval(js).is_ok(),
            _ => false,
        }
    }

    /// 读取站点 webview 当前 URL 的 fragment（eval_with_ack 之外的诊断/验证
    /// 路径共用：eval 探针把结果写进 hash——SPA 改 hash 不触发导航）
    pub fn url_fragment(&self, app: &AppHandle<Wry>, site_id: &str) -> Option<String> {
        let wv = self.get(site_id)?;
        // WKWebView 首次提交导航前 URL 为 nil，wry 的 url() 会 unwrap panic
        if !self.is_loaded(site_id) {
            return None;
        }
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// pending_reload 的消费语义：标记后首载完成消费一次（触发补 reload），
    /// 之后再次加载完成不得再消费（reload 自身也会触发 Finished，防循环）。
    /// reload_loaded 对未就绪站点的标记路径需要真实 webview 句柄，无法单测，
    /// 这里直接对标记集合操作验证 consume 端语义。
    #[test]
    fn pending_reload_consumed_once() {
        let tabs = Tabs::new();
        tabs.pending_reload
            .lock()
            .unwrap()
            .insert("kimi".to_string());
        assert!(tabs.consume_pending_reload("kimi")); // 首次：补 reload
        assert!(!tabs.consume_pending_reload("kimi")); // reload 后的 Finished：不再补
        assert!(!tabs.consume_pending_reload("chatgpt")); // 未标记的站点
    }

    /// pending_ls 同语义：消费一次返回脚本，之后为 None（防注入-reload 循环）
    #[test]
    fn pending_ls_consumed_once() {
        let tabs = Tabs::new();
        tabs.pending_ls
            .lock()
            .unwrap()
            .insert("kimi".to_string(), "js-here".to_string());
        assert_eq!(tabs.consume_pending_ls("kimi"), Some("js-here".to_string()));
        assert_eq!(tabs.consume_pending_ls("kimi"), None);
        assert_eq!(tabs.consume_pending_ls("chatgpt"), None);
    }

    /// 注入脚本条目计数：认 build_ls_inject_js 首行的 `// entries: N` 注释
    #[test]
    fn ls_scripts_entry_count_parses_header() {
        assert_eq!(ls_scripts_entry_count("// entries: 42\n(function(){})()"), 42);
        assert_eq!(ls_scripts_entry_count("// entries: 0\n(function(){})()"), 0);
        assert_eq!(ls_scripts_entry_count("(function(){})()"), 0); // 无头
        assert_eq!(ls_scripts_entry_count("// entries: abc\n()"), 0); // 非数字
    }
}
