mod commands;
mod inject;
mod sites;
mod store;
mod tabs;

use tauri::Manager;

/// 全局站点注册表（setup 时加载一次）
pub struct Sites(pub Vec<sites::SiteConfig>);

/// SQLite 存储
pub struct Db(pub store::Store);

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_clipboard_manager::init())
        .setup(|app| {
            let sites = sites::load().expect("sites.toml 解析失败");
            app.manage(Sites(sites));
            app.manage(tabs::Tabs::new());

            let dir = app.path().app_config_dir()?;
            std::fs::create_dir_all(&dir)?;
            let db = store::Store::open(&dir.join("fama.db"))?;
            app.manage(Db(db));

            // macOS：tauri 默认 TitleBarStyle::Visible 会给窗口开 FullSizeContentView
            // （contentView 顶到系统标题栏底下）。子 WKWebView 的页面布局视口会被
            // 减去标题栏高度的安全区内缩（实测 32pt），渲染出上下留白、透出占位网格。
            // 关掉该标志位，contentView 回到标题栏以下，安全区内缩消失。
            #[cfg(target_os = "macos")]
            {
                use objc2_app_kit::{NSWindow, NSWindowStyleMask};
                let webview = app.get_webview("main").expect("main webview 不存在");
                webview
                    .with_webview(|wv| unsafe {
                        let nsw = &*(wv.ns_window() as *mut NSWindow);
                        let mut mask = nsw.styleMask();
                        mask.set(NSWindowStyleMask::FullSizeContentView, false);
                        nsw.setStyleMask(mask);
                    })
                    .expect("关闭 FullSizeContentView 失败");
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_sites,
            commands::preload_tabs,
            commands::layout_update,
            commands::activate_tab,
            commands::dispatch_question,
            commands::list_rounds,
            commands::rate,
        ])
        .run(tauri::generate_context!())
        .expect("error while running fama");
}
