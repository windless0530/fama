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

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_sites,
            commands::layout_update,
            commands::activate_tab,
            commands::dispatch_question,
            commands::list_rounds,
            commands::rate,
        ])
        .run(tauri::generate_context!())
        .expect("error while running fama");
}
