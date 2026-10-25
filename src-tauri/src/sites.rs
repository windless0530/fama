//! 站点注册表：sites.toml + 注入脚本 的加载与校验（design.md §3）

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
struct SitesFile {
    site: Vec<RawSite>,
}

#[derive(Debug, Deserialize)]
struct RawSite {
    id: String,
    name: String,
    url: String,
    icon: String,
    injector: String,
    /// 该站点登录态依赖 localStorage（如 kimi 的 token 不在 cookie），
    /// 导入流程除 cookie 外还要读浏览器 localStorage 并注入 webview
    #[serde(default)]
    import_local_storage: bool,
}

/// 前端可见的站点信息（IPC 契约）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SiteInfo {
    pub id: String,
    pub name: String,
    pub icon: String,
}

/// 内部完整配置
#[derive(Debug, Clone)]
pub struct SiteConfig {
    pub id: String,
    pub name: String,
    pub url: url::Url,
    pub icon: String,
    /// 注入脚本全文（编译期打入）
    pub injector_src: &'static str,
    /// 脚本头 `// @version N`，落库用于排查
    pub injector_ver: String,
    /// 站点登录态依赖 localStorage（sites.toml `import_local_storage`）
    pub import_local_storage: bool,
}

/// 注入脚本注册：新增站点在这里加一行 include_str!
const INJECTORS: &[(&str, &str)] = &[
    ("_generic", include_str!("../injectors/_generic.js")),
    ("chatgpt", include_str!("../injectors/chatgpt.js")),
    ("glm", include_str!("../injectors/glm.js")),
    ("deepseek", include_str!("../injectors/deepseek.js")),
    ("kimi", include_str!("../injectors/kimi.js")),
];

fn injector_version(src: &str) -> String {
    for line in src.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("// @version ") {
            return rest.trim().to_string();
        }
        if !t.starts_with("//") {
            break; // 版本声明只认脚本头部注释
        }
    }
    "0".to_string()
}

pub fn load() -> Result<Vec<SiteConfig>, String> {
    let raw: SitesFile =
        toml::from_str(include_str!("../sites.toml")).map_err(|e| e.to_string())?;
    let raw = raw.site;

    let mut out = Vec::with_capacity(raw.len());
    for r in raw {
        if r.id.is_empty()
            || !r
                .id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(format!("非法站点 id: {:?}", r.id));
        }
        if out.iter().any(|s: &SiteConfig| s.id == r.id) {
            return Err(format!("重复站点 id: {}", r.id));
        }
        let url = url::Url::parse(&r.url).map_err(|e| format!("站点 {} url 非法: {}", r.id, e))?;
        let (_, src) = INJECTORS
            .iter()
            .find(|(name, _)| *name == r.injector)
            .ok_or_else(|| format!("站点 {} 引用了未注册的注入脚本 {}", r.id, r.injector))?;
        out.push(SiteConfig {
            id: r.id,
            name: r.name,
            url,
            icon: r.icon,
            injector_src: src,
            injector_ver: injector_version(src),
            import_local_storage: r.import_local_storage,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_all_sites() {
        let sites = load().unwrap();
        assert_eq!(sites.len(), 4);
        assert!(sites.iter().any(|s| s.id == "chatgpt"));
    }

    /// localStorage 导入是按站点配置的通用能力：当前只有 kimi 打开
    #[test]
    fn local_storage_import_is_per_site() {
        let sites = load().unwrap();
        let kimi = sites.iter().find(|s| s.id == "kimi").unwrap();
        assert!(kimi.import_local_storage);
        for s in &sites {
            if s.id != "kimi" {
                assert!(!s.import_local_storage, "{} 不应开 localStorage 导入", s.id);
            }
        }
    }

    #[test]
    fn every_injector_has_contract_fns() {
        for (_, src) in INJECTORS {
            assert!(src.contains("async function famaFill"), "缺 famaFill");
            assert!(src.contains("async function famaSend"), "缺 famaSend");
        }
    }
}
