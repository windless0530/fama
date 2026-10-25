//! 站点 webview 管理（design.md §2.2）。
//!
//! M0 实现 desktop 路径：Tauri v2 子 webview（wry）。
//! iOS 路线（Tauri 多 webview 或 FamaTabs Swift 插件）在 M0 spike 后落进本模块，
//! 上层接口保持不变。
//!
//! webview 句柄由本模块自管注册表持有（创建后插入），不依赖运行时枚举 API。
//!
//! localStorage 注入任务（design.md §2.5.2）：任务带预期 origin / generation /
//! running 状态注册，调度前做 origin 门禁（native URL 检查 + 脚本内
//! `location.origin` 检查 + ACK 回读时的 origin 复核三层），结果只认
//! 页面写回 URL fragment 的真实回执——eval 派发成功不算注入成功。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// 当前停在已完成加载页面的站点。
    /// macOS 上 wry 的 url() 对未加载的 WKWebView 会 panic（URL 为 nil 时 unwrap），
    /// 读取 URL / eval 前必须先确认页面就绪。导航 Started 时撤下、Finished 时
    /// 重新置位——导航期间不得沿用旧页面的就绪状态。
    loaded: Mutex<HashSet<String>>,
    /// 当前处于"可见"状态的站点（apply_layout 显示后插入、隐藏时移除）。
    /// 从隐藏恢复显示时 WKWebView 的远端图层可能停留在旧几何（隐藏期间的
    /// frame 变化不进图层），需要跨 CA 事务的分步重设，见 apply_layout。
    shown: Mutex<HashSet<String>>,
    /// cookie 导入注入时仍未完成首次加载的站点：其导航请求早于注入发出、
    /// 不带新 cookie，首次加载完成（on_page_load Finished）时补一次 reload。
    /// consume 与 insert 都以 pending_reload 锁串行化，reload 只触发一次。
    /// localStorage 站点不使用本标记（其 reload 由注入任务成功后统一触发，
    /// finalize 时顺带消费本标记，一次刷新同时生效 cookie 与 localStorage）。
    pending_reload: Mutex<HashSet<String>>,
    /// localStorage 注入任务：site_id → 任务（预期 origin、脚本、generation、
    /// running）。任务在成功确认（合法 ACK）或被新导入替换前保留；失败 /
    /// 页面暂不在站点 origin 时不消费，等下一次页面加载完成自动重试。
    ls_jobs: Mutex<HashMap<String, LsJob>>,
    /// 创建互斥：preload / layout_update / dispatch 可能并发 ensure_tab，
    /// 注册表插入在创建之后，需要串行化 + 双重检查防止重复创建。
    creating: Mutex<()>,
}

/// 全局任务代数：每次注册任务递增，用于作废旧 worker 的迟到结果与事件。
/// 同时是前端「按站点记录最新 generation，忽略旧事件」的依据。
static LS_GENERATION: AtomicU64 = AtomicU64::new(0);

// ---------- localStorage 注入任务（跨平台共享；脚本内容由平台实现构造） ----------

/// 导入流程注册一次注入任务的输入。`script` 由平台实现（macOS
/// browser_session）生成，本模块不感知其内容，只负责调度与回执校验。
#[derive(Debug, Clone)]
pub struct LsJobSpec {
    /// 预期 origin（`scheme://host[:port]`，来自站点配置 URL 的规范化形式）。
    /// 注入只允许落在精确匹配的站点 origin（三层校验见 LsJob 注释）。
    pub expected_origin: String,
    /// 注入脚本文本（内嵌键值、nonce 与 origin 自检）
    pub script: String,
    /// 脚本内嵌的 ACK nonce（回执匹配用；与 generation 相互独立）
    pub nonce: u64,
    /// 注入条目数（校验回执的 attempted 必须等于它）
    pub entries: usize,
}

/// 注册后的任务快照（worker / 内联执行路径共用）
#[derive(Debug, Clone)]
pub struct LsJobSnapshot {
    pub generation: u64,
    pub spec: LsJobSpec,
}

/// 任务本体（注册表内的可变状态）
#[derive(Debug, Clone)]
struct LsJob {
    spec: LsJobSpec,
    generation: u64,
    /// 有 worker 正在执行：调度入口据此防重入
    running: bool,
}

/// 单站点一次注入尝试的结果
#[derive(Debug, Clone, PartialEq)]
pub enum LsAttemptResult {
    /// 合法 ACK 确认：written 条写入且回读一致，failed 条写入/回读失败
    Confirmed { written: usize, failed: usize },
    /// eval 派发失败 / 回执超时 / 回执校验失败（任务保留可重试）
    Failed(LsFailKind),
    /// 页面未就绪或不在站点 origin：本次不 eval，任务保留等下次调度
    RetryLater,
}

/// 注入失败类别（固定文案，不含键值与异常原文）
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LsFailKind {
    /// eval 派发失败
    Eval,
    /// 回执超时（页面未写回 fragment：被导航吞掉 / 站点改写 hash / 页面挂起）
    AckTimeout,
    /// 回执存在但 nonce / origin / 计数校验失败
    MalformedAck,
}

impl LsFailKind {
    /// 站点级可见错误文案（固定类别 → 固定文案）
    pub fn label(self) -> &'static str {
        match self {
            LsFailKind::Eval => "注入脚本派发失败",
            LsFailKind::AckTimeout => "注入回执超时（页面可能正在跳转），稍后自动重试",
            LsFailKind::MalformedAck => "注入回执校验失败",
        }
    }
}

/// 尝试收尾动作（调用方按此决定 reload / 事件 / 计数）
#[derive(Debug, PartialEq)]
pub enum LsFinalize {
    /// ACK 确认（含部分成功）：任务已移除，调用方应 reload 站点
    Confirmed,
    /// 任务保留（失败可重试 / 页面暂不可注入）：不 reload
    Retained,
    /// generation 已被更新任务取代：调用方不得计数、发事件或 reload
    Stale,
}

/// 单站点 localStorage 导入结果（ImportReport 字段与 `browser-import-ls-result`
/// 事件同形；不含任何键名、值或脚本内容）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LsSiteReport {
    pub site_id: String,
    pub generation: u64,
    /// 从浏览器 localStorage 读到的条目数
    pub read: usize,
    /// 已确认写入（写后回读一致）的条数
    pub injected: usize,
    /// 尚未注入（等待页面就绪 / 任务保留待重试）的条数
    pub pending: usize,
    /// 确认写入失败的条数（写异常或回读不一致）
    pub failed: usize,
    /// ok = 全部确认写入；partial = 部分写入；failed = 注入尝试失败（可重试）；
    /// pending = 尚未注入（等待页面）
    pub status: &'static str,
    /// 站点级错误文案（固定类别）
    pub error: Option<String>,
}

pub const LS_STATUS_OK: &str = "ok";
pub const LS_STATUS_PARTIAL: &str = "partial";
pub const LS_STATUS_FAILED: &str = "failed";
pub const LS_STATUS_PENDING: &str = "pending";

/// 由尝试结果生成站点报告（注入路径）；挂起路径直接用 [`LsSiteReport::pending`]
pub fn ls_report_from_result(
    site_id: &str,
    snap: &LsJobSnapshot,
    result: &LsAttemptResult,
) -> LsSiteReport {
    let entries = snap.spec.entries;
    match result {
        LsAttemptResult::Confirmed { written, failed } => {
            let (status, error) = if *failed == 0 {
                (LS_STATUS_OK, None)
            } else if *written > 0 {
                (
                    LS_STATUS_PARTIAL,
                    Some(format!("{failed} 条写入失败（站点拒绝或存储配额不足）")),
                )
            } else {
                (
                    LS_STATUS_FAILED,
                    Some("全部条目写入失败（站点拒绝或存储配额不足）".to_string()),
                )
            };
            LsSiteReport {
                site_id: site_id.to_string(),
                generation: snap.generation,
                read: entries,
                injected: *written,
                pending: 0,
                failed: *failed,
                status,
                error,
            }
        }
        LsAttemptResult::Failed(kind) => LsSiteReport {
            site_id: site_id.to_string(),
            generation: snap.generation,
            read: entries,
            injected: 0,
            // 任务保留：条目仍未注入，等待重试
            pending: entries,
            failed: 0,
            status: LS_STATUS_FAILED,
            error: Some(kind.label().to_string()),
        },
        LsAttemptResult::RetryLater => LsSiteReport::pending(site_id, snap.generation, entries),
    }
}

impl LsSiteReport {
    /// 挂起（等待页面就绪）形态的报告
    pub fn pending(site_id: &str, generation: u64, read: usize) -> Self {
        Self {
            site_id: site_id.to_string(),
            generation,
            read,
            injected: 0,
            pending: read,
            failed: 0,
            status: LS_STATUS_PENDING,
            error: None,
        }
    }
}

/// 站点报告求和（ImportReport 聚合字段的数据来源；单站失败不会被其他站
/// 的成功抵消——failed / status 逐站保留在 ls_sites 里）
#[derive(Debug, Default, PartialEq)]
pub struct LsTotals {
    pub read: usize,
    pub injected: usize,
    pub pending: usize,
    pub failed: usize,
}

pub fn ls_totals(reports: &[LsSiteReport]) -> LsTotals {
    reports.iter().fold(LsTotals::default(), |mut t, r| {
        t.read += r.read;
        t.injected += r.injected;
        t.pending += r.pending;
        t.failed += r.failed;
        t
    })
}

// ---------- origin 规范化与比较（可单测的安全边界） ----------

/// URL → 规范 origin 串（`scheme://host[:port]`，host 小写、默认端口省略）。
/// 与浏览器 `location.origin`、Chromium localStorage 键里的 origin 同形。
/// 无 host 的 URL（about:blank 等）返回 None。
pub fn url_origin(u: &url::Url) -> Option<String> {
    let host = u.host_str()?.to_ascii_lowercase();
    let port = match u.port() {
        Some(p) => format!(":{p}"),
        None => String::new(),
    };
    Some(format!("{}://{host}{port}", u.scheme()))
}

/// 当前 URL 是否精确位于预期 origin（scheme + host + 有效端口全等）。
/// path / query / fragment 变化不影响；http↔https、不同端口、裸域↔www、
/// 相似后缀域、SSO / 外部域一律不匹配。
pub fn origin_matches(u: &url::Url, expected: &str) -> bool {
    url_origin(u).as_deref() == Some(expected)
}

// ---------- ACK 回执解析（纯函数，可单测） ----------

/// 脚本写回的回执 JSON 形状（只含计数与固定类别，无键值）
#[derive(Debug, Deserialize)]
struct LsAckJson {
    nonce: u64,
    origin: String,
    attempted: usize,
    written: usize,
    failed: usize,
    #[serde(default)]
    error: String,
}

/// 校验通过的回执
#[derive(Debug, PartialEq)]
pub struct LsAck {
    pub written: usize,
    pub failed: usize,
}

/// 注入脚本的固定错误类别（脚本只允许回传这三种）
const LS_ACK_ERRORS: &[&str] = &["", "write", "script"];

/// 解析并校验注入回执（fragment 里 `fama-ls-ack=<nonce>:` 之后的部分，
/// percent-encoded JSON）。校验全部通过才返回 Some：
/// - JSON 可解析、字段齐全；
/// - nonce、origin 精确匹配；
/// - `attempted == entries`、`written + failed == attempted`（防伪造计数）；
/// - 错误类别属于固定集合。
pub fn parse_ls_ack(
    encoded: &str,
    nonce: u64,
    expected_origin: &str,
    entries: usize,
) -> Option<LsAck> {
    let decoded = percent_encoding::percent_decode_str(encoded)
        .decode_utf8()
        .ok()?;
    let ack: LsAckJson = serde_json::from_str(&decoded).ok()?;
    if ack.nonce != nonce || ack.origin != expected_origin {
        return None;
    }
    if ack.attempted != entries || ack.written + ack.failed != ack.attempted {
        return None;
    }
    if !LS_ACK_ERRORS.contains(&ack.error.as_str()) {
        return None;
    }
    Some(LsAck {
        written: ack.written,
        failed: ack.failed,
    })
}

// ---------- 回执等待参数 ----------

/// ACK 等待上限（对齐 dispatch 路径的 ACK_TIMEOUT）
const LS_ACK_TIMEOUT: Duration = Duration::from_millis(3000);
/// ACK fragment 轮询间隔
const LS_ACK_POLL_INTERVAL: Duration = Duration::from_millis(150);

impl Tabs {
    pub fn new() -> Self {
        Self {
            webviews: Mutex::new(HashMap::new()),
            rects: Mutex::new(HashMap::new()),
            loaded: Mutex::new(HashSet::new()),
            shown: Mutex::new(HashSet::new()),
            pending_reload: Mutex::new(HashSet::new()),
            ls_jobs: Mutex::new(HashMap::new()),
            creating: Mutex::new(()),
        }
    }

    pub fn get(&self, site_id: &str) -> Option<Webview<Wry>> {
        self.webviews.lock().unwrap().get(site_id).cloned()
    }

    fn mark_loaded(&self, site_id: &str) {
        self.loaded.lock().unwrap().insert(site_id.to_string());
    }

    /// 导航开始：撤下就绪标记（页面可能正离开站点 origin，URL 读取 / eval /
    /// 注入调度都必须等新页面 Finished 之后）
    fn clear_loaded(&self, site_id: &str) {
        self.loaded.lock().unwrap().remove(site_id);
    }

    fn is_loaded(&self, site_id: &str) -> bool {
        self.loaded.lock().unwrap().contains(site_id)
    }

    /// reload 已完成首次加载的站点 webview（浏览器登录态导入后刷新用），
    /// 并注册 localStorage 注入任务：
    /// - cookie-only 已加载站点：立即 reload（与旧路径一致）；
    /// - cookie-only 未完成首载站点：挂 pending_reload（首载 Finished 补 reload）；
    /// - localStorage 站点（无论 webview 是否创建 / 是否完成首载）：注册任务。
    ///   已加载的返回快照由调用方（导入线程，非主线程）立即内联执行；
    ///   未就绪的保持挂起，页面 Finished 时由后台 worker 执行。
    ///   任务成功确认后 reload（同时让 cookie 生效），失败保留待重试；
    /// - 未创建 webview 的 cookie-only 站点无需处理（创建时数据已在）。
    ///
    /// `ls_specs` 为空时行为与纯 cookie 导入完全一致。
    pub fn reload_loaded(
        &self,
        site_ids: &[String],
        ls_specs: &HashMap<String, LsJobSpec>,
    ) -> ImportReload {
        let mut out = ImportReload::default();
        // 锁序：loaded → webviews → pending_reload → ls_jobs（整段持有，
        // 「检查已加载 + 注册任务 / 挂 reload 标记」对主线程的 on_page_load
        // 回调原子；on_page_loaded / finalize 均只单独取锁，无反向嵌套）
        let loaded = self.loaded.lock().unwrap();
        let webviews = self.webviews.lock().unwrap();
        let mut pending = self.pending_reload.lock().unwrap();
        let mut ls_jobs = self.ls_jobs.lock().unwrap();
        for id in site_ids {
            let loaded_now = loaded.contains(id);
            if let Some(spec) = ls_specs.get(id) {
                let running = loaded_now && webviews.contains_key(id);
                let snap = insert_ls_job(&mut ls_jobs, id, spec, running);
                if running {
                    out.ls_immediate.push((id.clone(), snap));
                } else {
                    out.ls_pending
                        .push(LsSiteReport::pending(id, snap.generation, spec.entries));
                }
            } else if let Some(wv) = webviews.get(id) {
                if loaded_now {
                    if wv.eval("location.reload()").is_ok() {
                        out.reloaded.push(id.clone());
                    }
                } else {
                    pending.insert(id.clone());
                }
            }
        }
        out
    }

    /// 首次加载完成时消费 pending_reload 标记：存在则返回 true（调用方应 reload）。
    /// remove 语义保证同一站点一次导入只补一次 reload，不会循环。
    fn consume_pending_reload(&self, site_id: &str) -> bool {
        self.pending_reload.lock().unwrap().remove(site_id)
    }

    fn has_ls_job(&self, site_id: &str) -> bool {
        self.ls_jobs.lock().unwrap().contains_key(site_id)
    }

    /// 调度入口：站点存在未在执行的任务时置 running 并返回快照（调用方据此
    /// 执行注入）。无任务 / 正在执行 / 页面未加载时返回 None（等待，不消费）。
    fn prepare_ls_schedule(&self, site_id: &str) -> Option<LsJobSnapshot> {
        let mut jobs = self.ls_jobs.lock().unwrap();
        let job = jobs.get_mut(site_id)?;
        if job.running {
            return None;
        }
        job.running = true;
        Some(LsJobSnapshot {
            generation: job.generation,
            spec: job.spec.clone(),
        })
    }

    /// 执行一次注入尝试：origin 门禁（native）→ eval → 有界等待 ACK。
    /// **只允许在非主线程调用**（内部 sleep 轮询；主线程路径经
    /// spawn_ls_worker 转后台线程）。eval 派发成功不算成功——结果只认
    /// 页面写回的合法 ACK。
    pub fn run_ls_attempt(
        &self,
        app: &AppHandle<Wry>,
        site_id: &str,
        snap: &LsJobSnapshot,
    ) -> LsAttemptResult {
        // 门禁 1：页面就绪 + 当前 URL 的 origin 精确匹配（不匹配不 eval，
        // 任务保留等页面回到站点 origin——绝不把 token 写进外部 origin）
        if !self.is_loaded(site_id) {
            return LsAttemptResult::RetryLater;
        }
        let Some((url, _)) = self.url_with_fragment(app, site_id) else {
            return LsAttemptResult::RetryLater;
        };
        if !origin_matches(&url, &snap.spec.expected_origin) {
            eprintln!(
                "[tabs] {site_id} ls inject deferred: page origin != {} (task kept)",
                snap.spec.expected_origin
            );
            return LsAttemptResult::RetryLater;
        }
        let Some(wv) = self.get(site_id) else {
            return LsAttemptResult::RetryLater;
        };
        // 门禁 2 在脚本内：location.origin 不匹配时不触碰 localStorage 也不回执
        if wv.eval(&snap.spec.script).is_err() {
            return LsAttemptResult::Failed(LsFailKind::Eval);
        }
        let prefix = format!("fama-ls-ack={}:", snap.spec.nonce);
        let deadline = Instant::now() + LS_ACK_TIMEOUT;
        loop {
            if let Some((url, frag)) = self.url_with_fragment(app, site_id) {
                if let Some(rest) = frag.strip_prefix(&prefix) {
                    // 门禁 3：ACK 只在页面仍位于预期 origin 时接受
                    //（防导航后残留 hash / 伪造回执）
                    if origin_matches(&url, &snap.spec.expected_origin) {
                        return match parse_ls_ack(
                            rest,
                            snap.spec.nonce,
                            &snap.spec.expected_origin,
                            snap.spec.entries,
                        ) {
                            Some(ack) => LsAttemptResult::Confirmed {
                                written: ack.written,
                                failed: ack.failed,
                            },
                            None => LsAttemptResult::Failed(LsFailKind::MalformedAck),
                        };
                    }
                }
            }
            if Instant::now() >= deadline {
                return LsAttemptResult::Failed(LsFailKind::AckTimeout);
            }
            std::thread::sleep(LS_ACK_POLL_INTERVAL);
        }
    }

    /// 尝试收尾：generation 匹配的任务按结果更新（成功移除 + 消费
    /// pending_reload；失败/放弃保留并清 running）。generation 不匹配返回
    /// Stale——旧 worker 不得计数、发事件或 reload。
    pub fn finalize_ls_attempt(
        &self,
        site_id: &str,
        generation: u64,
        result: &LsAttemptResult,
    ) -> LsFinalize {
        let confirmed = matches!(result, LsAttemptResult::Confirmed { .. });
        let mut jobs = self.ls_jobs.lock().unwrap();
        let Some(job) = jobs.get_mut(site_id) else {
            return LsFinalize::Stale;
        };
        if job.generation != generation {
            return LsFinalize::Stale;
        }
        if confirmed {
            jobs.remove(site_id);
            drop(jobs);
            // 一次 reload 同时让 localStorage 与 cookie 生效；remove 语义防循环
            let _ = self.consume_pending_reload(site_id);
            LsFinalize::Confirmed
        } else {
            job.running = false;
            LsFinalize::Retained
        }
    }

    /// 页面加载完成（on_page_load Finished，主线程）：
    /// - 有 localStorage 任务：调度后台 worker（主线程不同步等待）；
    /// - 否则有 pending_reload：补一次 reload。
    fn on_page_loaded(&self, wv: &Webview<Wry>) {
        let site_id = wv.label().to_string();
        if self.has_ls_job(&site_id) {
            if let Some(snap) = self.prepare_ls_schedule(&site_id) {
                eprintln!(
                    "[tabs] {site_id} ls job scheduled (gen {}, entries {})",
                    snap.generation, snap.spec.entries
                );
                spawn_ls_worker(wv.app_handle().clone(), site_id, snap);
            }
            // running 中：不动 pending_reload（任务成功路径统一处理 reload）
            return;
        }
        if self.consume_pending_reload(&site_id) {
            eprintln!("[tabs] {site_id} post-import reload");
            let _ = wv.eval("location.reload()");
        }
    }
}

/// 注册（或替换）任务：generation 全局递增，替换即作废旧任务与其 worker。
fn insert_ls_job(
    map: &mut HashMap<String, LsJob>,
    site_id: &str,
    spec: &LsJobSpec,
    running: bool,
) -> LsJobSnapshot {
    let generation = LS_GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
    map.insert(
        site_id.to_string(),
        LsJob {
            spec: spec.clone(),
            generation,
            running,
        },
    );
    LsJobSnapshot {
        generation,
        spec: spec.clone(),
    }
}

/// 后台 worker：执行尝试 → 收尾 → （非 Stale 时）发站点结果事件。
/// 主线程只负责 spawn，不同步等待结果。
fn spawn_ls_worker(app: AppHandle<Wry>, site_id: String, snap: LsJobSnapshot) {
    std::thread::spawn(move || {
        let tabs = app.state::<Tabs>();
        let result = tabs.run_ls_attempt(&app, &site_id, &snap);
        let fin = tabs.finalize_ls_attempt(&site_id, snap.generation, &result);
        eprintln!(
            "[tabs] {site_id} ls attempt done: gen={} result={result:?} finalize={fin:?}",
            snap.generation
        );
        if fin == LsFinalize::Confirmed {
            if let Some(wv) = tabs.get(&site_id) {
                let _ = wv.eval("location.reload()");
            }
        }
        if fin != LsFinalize::Stale {
            emit_ls_result(&app, &ls_report_from_result(&site_id, &snap, &result));
        }
    });
}

/// 前端事件：`browser-import-ls-result`，payload 为 LsSiteReport（无敏感数据）
fn emit_ls_result(app: &AppHandle<Wry>, report: &LsSiteReport) {
    use tauri::Emitter;
    if let Err(e) = app.emit("browser-import-ls-result", report) {
        eprintln!("[tabs] ls result event emit failed: {e}");
    }
}

/// reload_loaded 的结果（browser_session 拼进 ImportReport）
#[derive(Default)]
pub struct ImportReload {
    /// 实际立即触发 reload 的 cookie-only 站点（localStorage 站点在其注入
    /// 确认后由导入方/worker reload，也计入 ImportReport.reloaded）
    pub reloaded: Vec<String>,
    /// 已加载页面上的注入任务快照：调用方（导入线程，非主线程）内联执行
    /// `run_ls_attempt` + `finalize_ls_attempt`
    pub ls_immediate: Vec<(String, LsJobSnapshot)>,
    /// 挂起待页面就绪的任务的站点报告（直接进 ImportReport.ls_sites）
    pub ls_pending: Vec<LsSiteReport>,
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
                    let tabs = wv.state::<Tabs>();
                    match payload.event() {
                        // 导航开始撤下就绪标记：注入调度 / URL 读取等新页面
                        // Finished 后再进行（防在导航竞态里对错误 origin 操作）
                        PageLoadEvent::Started => tabs.clear_loaded(wv.label()),
                        PageLoadEvent::Finished => {
                            tabs.mark_loaded(wv.label());
                            // 导入后才完成首载的页面：导航请求早于注入发出。
                            // localStorage 任务优先（成功后的 reload 一次同时
                            // 生效 cookie 与 localStorage）；否则按 cookie 路径
                            // 补 reload。consume 的 remove 语义防循环。
                            tabs.on_page_loaded(&wv);
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
    tabs.url_with_fragment(app, site_id)
        .map(|(_, f)| f)
}

impl Tabs {
    /// 读取站点 webview 当前完整 URL 与其 fragment（读取派发到主线程，同步
    /// 等待；页面未完成加载时返回 None——wry 对未加载页面的 url() 会 panic）。
    /// 注入 ACK 轮询用它同时复核 origin。
    pub fn url_with_fragment(
        &self,
        app: &AppHandle<Wry>,
        site_id: &str,
    ) -> Option<(url::Url, String)> {
        let wv = self.get(site_id)?;
        if !self.is_loaded(site_id) {
            return None;
        }
        let (tx, rx) = mpsc::channel();
        app.run_on_main_thread(move || {
            // 站点页 URL 形如 https://host/path#fama-ack=...；SPA 改 hash 不触发导航
            let _ = tx.send(
                wv.url().ok().map(|u| {
                    (
                        u.clone(),
                        u.fragment().map(|s| s.to_string()).unwrap_or_default(),
                    )
                }),
            );
        })
        .ok()?;
        rx.recv_timeout(Duration::from_millis(500)).ok().flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(origin: &str, nonce: u64, entries: usize) -> LsJobSpec {
        LsJobSpec {
            expected_origin: origin.to_string(),
            script: format!("js-{nonce}"),
            nonce,
            entries,
        }
    }

    /// pending_reload 的消费语义：标记后首载完成消费一次（触发补 reload），
    /// 之后再次加载完成不得再消费（reload 自身也会触发 Finished，防循环）。
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

    // ---------- origin 规范化与门禁 ----------

    /// 规范 origin：host 小写、默认端口省略、显式端口保留、path/query/hash 无关
    #[test]
    fn url_origin_canonicalizes() {
        let o = |s: &str| url::Url::parse(s).ok().and_then(|u| url_origin(&u));
        assert_eq!(
            o("https://www.kimi.com/").as_deref(),
            Some("https://www.kimi.com")
        );
        // 默认端口 443 由 url crate 归一化省略；path/query/hash 不参与
        assert_eq!(
            o("https://www.kimi.com:443/chat?q=1#frag").as_deref(),
            Some("https://www.kimi.com")
        );
        assert_eq!(
            o("https://example.com:8443/").as_deref(),
            Some("https://example.com:8443")
        );
        // host 大小写归一
        assert_eq!(
            o("https://WWW.Kimi.COM/").as_deref(),
            Some("https://www.kimi.com")
        );
        // 无 host 的 URL 无 origin
        assert_eq!(o("about:blank"), None);
        assert_eq!(o("data:text/plain,hi"), None);
    }

    /// origin 门禁：仅 scheme+host+端口全等接受；http、端口、裸域/www、
    /// 相似后缀域、SSO / 外部域全部拒绝
    #[test]
    fn origin_gate_only_exact_match() {
        let expected = "https://www.kimi.com";
        let yes = |s: &str| {
            url::Url::parse(s)
                .ok()
                .is_some_and(|u| origin_matches(&u, expected))
        };
        // 同 origin 的 path / query / fragment 变化允许
        assert!(yes("https://www.kimi.com/"));
        assert!(yes("https://www.kimi.com/chat/history?page=2#x"));
        assert!(yes("https://www.kimi.com:443/"));
        // scheme 不同
        assert!(!yes("http://www.kimi.com/"));
        // 端口不同
        assert!(!yes("https://www.kimi.com:8443/"));
        // 裸域 / www 差异（不同 origin）
        assert!(!yes("https://kimi.com/"));
        assert!(!yes("https://m.kimi.com/"));
        // 相似后缀域（防钓鱼）
        assert!(!yes("https://www.kimi.com.evil.com/"));
        assert!(!yes("https://evil-kimi.com/"));
        // SSO / 外部域
        assert!(!yes("https://accounts.google.com/"));
        assert!(!yes("https://login.microsoftonline.com/"));
        // 空 / 坏 URL
        assert!(!yes(""));
        assert!(!yes("not a url"));
    }

    // ---------- ACK 解析 ----------

    fn ack_json(nonce: u64, origin: &str, a: usize, w: usize, f: usize, e: &str) -> String {
        let raw = format!(
            r#"{{"nonce":{nonce},"origin":"{origin}","attempted":{a},"written":{w},"failed":{f},"error":"{e}"}}"#
        );
        percent_encoding::utf8_percent_encode(
            &raw,
            percent_encoding::NON_ALPHANUMERIC,
        )
        .to_string()
    }

    /// 合法回执：全量成功 / 部分成功 / 全量失败都能解析出真实计数
    #[test]
    fn parse_ls_ack_accepts_valid() {
        let ok = ack_json(7, "https://www.kimi.com", 65, 65, 0, "");
        assert_eq!(
            parse_ls_ack(&ok, 7, "https://www.kimi.com", 65),
            Some(LsAck {
                written: 65,
                failed: 0
            })
        );
        let part = ack_json(7, "https://www.kimi.com", 65, 60, 5, "write");
        assert_eq!(
            parse_ls_ack(&part, 7, "https://www.kimi.com", 65),
            Some(LsAck {
                written: 60,
                failed: 5
            })
        );
        // 外层异常类别
        let script_err = ack_json(7, "https://www.kimi.com", 65, 0, 65, "script");
        assert_eq!(
            parse_ls_ack(&script_err, 7, "https://www.kimi.com", 65),
            Some(LsAck {
                written: 0,
                failed: 65
            })
        );
    }

    /// 错误 nonce / origin、attempted 不符、计数不闭合、未知错误类别、
    /// 畸形 JSON / percent 编码 → 一律拒绝（None）
    #[test]
    fn parse_ls_ack_rejects_invalid() {
        let good = ack_json(7, "https://www.kimi.com", 65, 65, 0, "");
        let p = |s: &str| parse_ls_ack(s, 7, "https://www.kimi.com", 65);
        assert!(p(&good).is_some());
        // nonce 不符
        assert_eq!(p(&ack_json(8, "https://www.kimi.com", 65, 65, 0, "")), None);
        // origin 不符（回执来自其他 origin 的页面）
        assert_eq!(p(&ack_json(7, "https://kimi.com", 65, 65, 0, "")), None);
        // attempted != 条目数
        assert_eq!(p(&ack_json(7, "https://www.kimi.com", 64, 64, 0, "")), None);
        // written + failed != attempted（伪造计数）
        assert_eq!(p(&ack_json(7, "https://www.kimi.com", 65, 60, 0, "")), None);
        assert_eq!(p(&ack_json(7, "https://www.kimi.com", 65, 66, 0, "")), None);
        // 未知错误类别
        assert_eq!(p(&ack_json(7, "https://www.kimi.com", 65, 0, 65, "weird")), None);
        // 畸形 JSON / 非法 percent 编码 / 缺字段
        assert_eq!(p("%7Bnope"), None);
        assert_eq!(p("%zz%ff"), None);
        assert_eq!(
            p(&percent_encoding::utf8_percent_encode(
                r#"{"nonce":7}"#,
                percent_encoding::NON_ALPHANUMERIC
            )
            .to_string()),
            None
        );
    }

    // ---------- 任务生命周期 ----------

    /// 成功路径：调度（置 running 防重入）→ ACK 确认 → 任务移除 +
    /// pending_reload 一并消费（一次 reload 同时生效 cookie 与 localStorage）；
    /// 移除后再次调度 / 收尾均为空 / Stale（reload 后的 Finished 不再注入，防循环）
    #[test]
    fn ls_job_success_consumes_once_and_reload_marker() {
        let tabs = Tabs::new();
        let snap = insert_ls_job(
            &mut tabs.ls_jobs.lock().unwrap(),
            "kimi",
            &spec("https://www.kimi.com", 7, 3),
            false,
        );
        // pending_reload 已挂（cookie 路径残留）：成功时一并消费
        tabs.pending_reload
            .lock()
            .unwrap()
            .insert("kimi".to_string());

        // 首次调度：running=false → 返回快照并置 running
        let scheduled = tabs.prepare_ls_schedule("kimi").unwrap();
        assert_eq!(scheduled.generation, snap.generation);
        // running 已置位：防重入
        assert!(tabs.prepare_ls_schedule("kimi").is_none());
        let fin = tabs.finalize_ls_attempt(
            "kimi",
            snap.generation,
            &LsAttemptResult::Confirmed { written: 3, failed: 0 },
        );
        assert_eq!(fin, LsFinalize::Confirmed);
        assert!(!tabs.has_ls_job("kimi")); // 任务已移除
        assert!(tabs.pending_reload.lock().unwrap().is_empty()); // 已消费
        assert!(tabs.prepare_ls_schedule("kimi").is_none()); // 无任务可调度
        assert_eq!(
            tabs.finalize_ls_attempt("kimi", snap.generation, &LsAttemptResult::Confirmed {
                written: 3, failed: 0
            }),
            LsFinalize::Stale // reload 触发的 Finished 再走收尾：Stale，不重复 reload
        );
    }

    /// 失败路径：任务保留、running 清除（可再次调度＝可重试）；
    /// pending_reload 不被消费（等成功后一并 reload）
    #[test]
    fn ls_job_failure_retains_task_for_retry() {
        let tabs = Tabs::new();
        let snap = insert_ls_job(
            &mut tabs.ls_jobs.lock().unwrap(),
            "kimi",
            &spec("https://www.kimi.com", 7, 3),
            true,
        );
        tabs.pending_reload
            .lock()
            .unwrap()
            .insert("kimi".to_string());
        assert_eq!(
            tabs.finalize_ls_attempt("kimi", snap.generation, &LsAttemptResult::Failed(
                LsFailKind::AckTimeout
            )),
            LsFinalize::Retained
        );
        assert!(tabs.has_ls_job("kimi"));
        assert!(!tabs.pending_reload.lock().unwrap().is_empty()); // 未消费
        // running 已清：可再次调度（重试）
        let snap2 = tabs.prepare_ls_schedule("kimi").unwrap();
        assert_eq!(snap2.generation, snap.generation);
        // RetryLater（页面未就绪 / origin 不符）同样保留
        assert_eq!(
            tabs.finalize_ls_attempt("kimi", snap.generation, &LsAttemptResult::RetryLater),
            LsFinalize::Retained
        );
        assert!(tabs.has_ls_job("kimi"));
    }

    /// 旧 worker 的迟到结果：generation 不匹配 → Stale，不得动新任务
    #[test]
    fn ls_job_stale_generation_does_not_touch_new_task() {
        let tabs = Tabs::new();
        let old = insert_ls_job(
            &mut tabs.ls_jobs.lock().unwrap(),
            "kimi",
            &spec("https://www.kimi.com", 7, 3),
            true,
        );
        // 新导入替换任务（generation 递增）
        let new = insert_ls_job(
            &mut tabs.ls_jobs.lock().unwrap(),
            "kimi",
            &spec("https://www.kimi.com", 8, 5),
            false,
        );
        assert_ne!(old.generation, new.generation);
        // 旧 worker 以「成功」收尾：Stale——新任务原样保留（未被移除）
        assert_eq!(
            tabs.finalize_ls_attempt("kimi", old.generation, &LsAttemptResult::Confirmed {
                written: 3, failed: 0
            }),
            LsFinalize::Stale
        );
        assert!(tabs.has_ls_job("kimi"));
        assert_eq!(tabs.prepare_ls_schedule("kimi").unwrap().spec.nonce, 8);
        // 任务消失后（无此站点）收尾也返回 Stale
        assert_eq!(
            tabs.finalize_ls_attempt("chatgpt", new.generation, &LsAttemptResult::RetryLater),
            LsFinalize::Stale
        );
    }

    // ---------- 站点报告 ----------

    /// 报告生成：ok / partial / 全量写入失败 / 尝试失败（可重试，条目仍 pending）
    /// / 放弃重试（pending 形态）
    #[test]
    fn ls_report_from_result_covers_all_statuses() {
        let snap = LsJobSnapshot {
            generation: 4,
            spec: spec("https://www.kimi.com", 7, 65),
        };
        let ok = ls_report_from_result("kimi", &snap, &LsAttemptResult::Confirmed {
            written: 65,
            failed: 0,
        });
        assert_eq!((ok.status, ok.injected, ok.pending, ok.failed), (LS_STATUS_OK, 65, 0, 0));
        assert!(ok.error.is_none());

        let part = ls_report_from_result("kimi", &snap, &LsAttemptResult::Confirmed {
            written: 60,
            failed: 5,
        });
        assert_eq!((part.status, part.injected, part.pending, part.failed), (LS_STATUS_PARTIAL, 60, 0, 5));
        assert!(part.error.is_some());

        let none = ls_report_from_result("kimi", &snap, &LsAttemptResult::Confirmed {
            written: 0,
            failed: 65,
        });
        assert_eq!((none.status, none.injected, none.failed), (LS_STATUS_FAILED, 0, 65));

        let failed = ls_report_from_result("kimi", &snap, &LsAttemptResult::Failed(
            LsFailKind::AckTimeout,
        ));
        assert_eq!(
            (failed.status, failed.injected, failed.pending, failed.failed),
            (LS_STATUS_FAILED, 0, 65, 0)
        );
        assert_eq!(failed.error.as_deref(), Some(LsFailKind::AckTimeout.label()));

        let pend = ls_report_from_result("kimi", &snap, &LsAttemptResult::RetryLater);
        assert_eq!((pend.status, pend.injected, pend.pending), (LS_STATUS_PENDING, 0, 65));
    }

    /// 聚合：逐站求和；单站失败不被其他站成功抵消（failed / status 站点级保留）
    #[test]
    fn ls_totals_sum_per_site() {
        let reports = vec![
            LsSiteReport {
                site_id: "kimi".into(),
                generation: 1,
                read: 65,
                injected: 60,
                pending: 0,
                failed: 5,
                status: LS_STATUS_PARTIAL,
                error: None,
            },
            LsSiteReport {
                site_id: "other".into(),
                generation: 2,
                read: 10,
                injected: 10,
                pending: 0,
                failed: 0,
                status: LS_STATUS_OK,
                error: None,
            },
            LsSiteReport::pending("third", 3, 7),
        ];
        let t = ls_totals(&reports);
        assert_eq!(
            t,
            LsTotals {
                read: 82,
                injected: 70,
                pending: 7,
                failed: 5
            }
        );
        // kimi 的失败仍在站点报告里可见
        assert!(reports.iter().any(|r| r.site_id == "kimi" && r.failed == 5));
    }
}
