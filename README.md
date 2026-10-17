# Fama

跨端（iOS / Windows / macOS）AI 聚合客户端：多 Tab 内嵌 ChatGPT / GLM / DeepSeek / Kimi 网页版，统一输入框一次输入、同时发到所有站点。

- 📖 产品目标与功能规格：[docs/feature.md](docs/feature.md)
- 🛠 技术设计：[docs/design.md](docs/design.md)
- 📱 M0 真机验证清单（iOS/Mac 侧）：[docs/m0-checklist.md](docs/m0-checklist.md)

## 快速开始（开发）

```bash
npm install
npm run tauri dev      # 桌面端开发模式（Linux/Windows/macOS）
npm run tauri ios init # 仅 macOS：生成 iOS 工程，随后 Xcode 打开 gen/apple 真机运行
```

依赖：Node 18+；桌面端另需 [Tauri 系统依赖](https://tauri.app/start/prerequisites/)；iOS 需要 macOS + Xcode 15+。

## 工程结构

```
src/                  前端（React + Vite + TS）：TabBar / ContentArea / InputBar / History
src-tauri/src/        Rust 核心：sites(站点注册) / tabs(webview管理) / inject(注入引擎) / store(SQLite)
src-tauri/injectors/  每站点注入脚本（famaFill / famaSend 契约）
src-tauri/sites.toml  站点注册表：新增 AI = 加一段配置 + 一个脚本
```

## 状态

🚧 M0 进行中：desktop 路径代码已就绪，待 iPhone 真机完成三个 spike（见 m0-checklist.md）。
