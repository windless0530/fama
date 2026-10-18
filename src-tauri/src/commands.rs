//! IPC 命令层（design.md §4）
//! 错误以 "E_XXX: 描述" 字符串形式返回前端。

use tauri::{AppHandle, Manager, State, Wry};

use crate::inject::{self, DispatchResult};
use crate::sites::SiteInfo;
use crate::store::Round;
use crate::tabs::{self, LayoutItem, Tabs};
use crate::{Db, Sites};

#[tauri::command]
pub fn list_sites(sites: State<Sites>) -> Vec<SiteInfo> {
    sites
        .0
        .iter()
        .map(|s| SiteInfo {
            id: s.id.clone(),
            name: s.name.clone(),
            icon: s.icon.clone(),
        })
        .collect()
}

#[tauri::command]
pub fn layout_update(
    app: AppHandle<Wry>,
    sites: State<Sites>,
    tabs_state: State<Tabs>,
    items: Vec<LayoutItem>,
) -> Result<(), String> {
    if items.is_empty() {
        return Err("E_EMPTY_LAYOUT: 布局为空".into());
    }
    for it in &items {
        if let Some(site) = sites.0.iter().find(|s| s.id == it.site_id) {
            tabs::ensure_tab(&app, &tabs_state, site)?;
        } else {
            return Err(format!("E_UNKNOWN_SITE: 未知站点 {}", it.site_id));
        }
    }
    tabs::apply_layout(&app, &tabs_state, &items)
}

/// 预建全部站点 webview（隐藏加载），让首次分发 / 首次切 tab 时页面已就绪。
/// 后台执行、立即返回；失败仅打日志。
#[tauri::command]
pub fn preload_tabs(app: AppHandle<Wry>, sites: State<Sites>) -> Result<(), String> {
    let all: Vec<crate::sites::SiteConfig> = sites.0.clone();
    tauri::async_runtime::spawn(async move {
        for site in &all {
            let tabs = app.state::<Tabs>();
            if let Err(e) = tabs::ensure_tab(&app, &tabs, site) {
                eprintln!("[preload] {} 预建失败: {}", site.id, e);
            }
        }
    });
    Ok(())
}

#[tauri::command]
pub fn activate_tab(
    app: AppHandle<Wry>,
    sites: State<Sites>,
    tabs_state: State<Tabs>,
    site_id: String,
) -> Result<(), String> {
    let site = sites
        .0
        .iter()
        .find(|s| s.id == site_id)
        .ok_or_else(|| format!("E_UNKNOWN_SITE: 未知站点 {site_id}"))?;
    tabs::ensure_tab(&app, &tabs_state, site)?;
    tabs::activate(&app, &tabs_state, &site_id)
}

#[tauri::command]
pub async fn dispatch_question(
    app: AppHandle<Wry>,
    sites: State<'_, Sites>,
    db: State<'_, Db>,
    question: String,
    site_ids: Vec<String>,
    auto_send: bool,
) -> Result<Vec<DispatchResult>, String> {
    let q = question.trim().to_string();
    if q.is_empty() {
        return Err("E_EMPTY_QUESTION: 问题为空".into());
    }
    let targets: Vec<_> = sites
        .0
        .iter()
        .filter(|s| site_ids.contains(&s.id))
        .cloned()
        .collect();
    if targets.is_empty() {
        return Err("E_NO_TARGETS: 未选择任何有效站点".into());
    }

    let round_id = db.0.insert_round(&q)?;
    for t in &targets {
        db.0.upsert_target(round_id, &t.id, "dispatching", None, None)?;
    }

    let results = inject::dispatch(&app, &targets, &q, auto_send).await;

    for r in &results {
        let ver = targets
            .iter()
            .find(|t| t.id == r.site_id)
            .map(|t| t.injector_ver.clone());
        db.0.upsert_target(
            round_id,
            &r.site_id,
            r.status,
            ver.as_deref(),
            r.error.as_deref(),
        )?;
    }
    Ok(results)
}

#[tauri::command]
pub async fn list_rounds(
    db: State<'_, Db>,
    limit: Option<i64>,
    before: Option<i64>,
) -> Result<Vec<Round>, String> {
    Ok(db
        .0
        .list_rounds(limit.unwrap_or(50).clamp(1, 200), before)?)
}

#[tauri::command]
pub async fn rate(
    db: State<'_, Db>,
    round_id: i64,
    site_id: String,
    rating: i64,
) -> Result<(), String> {
    if rating != 1 && rating != -1 {
        return Err("E_BAD_RATING: rating 只允许 1 或 -1".into());
    }
    db.0.set_rating(round_id, &site_id, rating)
}
