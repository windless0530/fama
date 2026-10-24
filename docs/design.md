# Fama · 技术设计文档

> 版本 v1.0（2026-10-17）· 对应产品目标：[feature.md](./feature.md)
>
> 本文档是技术方案的权威描述：架构、模块规格、数据契约、工程化与发布、里程碑与风险。

## 0. 决策摘要（讨论定稿）

| 决策项 | 结论 |
|---|---|
| 产品形态 | webview 聚合壳：多 Tab 内嵌 AI 网页版 + 统一输入一次多发 |
| 框架 | **Tauri v2**（Rust 核心 + React 前端 + 系统 webview） |
| 平台顺序 | **iOS 优先**（个人开发者 $99/年 + TestFlight 自用分发）→ Windows / macOS |
| 默认发送模式 | **自动发送**（输入框 Enter / 发送按钮 → 一键发到所有勾选站点） |
| 本地记录 | 保留：SQLite 存「问题轮次 + 站点目标 + 点赞/踩」，不含回答正文 |
| 与站点交互边界 | 只做用户触发的页面级 DOM 注入；不做抓包/协议逆向/后台自动化 |

## 1. 总体架构

### 1.1 分层

```
┌─────────────────── UI webview（React，App 自己的界面）────────────────────┐
│  TabBar · InputBar(统一输入) · SiteChips · HistoryPanel · Settings        │
└───────────────────────────────┬──────────────────────────────────────────┘
                         Tauri IPC（invoke）
┌───────────────────────────────┴──────────────────────────────────────────┐
│                            Rust 核心（src-tauri）                         │
│  sites.rs   站点注册表（sites.toml 加载校验）                              │
│  tabs.rs    WebviewHost 抽象 + 布局协商 + 平台实现路由                     │
│  inject.rs  注入引擎（fill → verify → send → ack，并行、超时、降级）        │
│  store.rs   SQLite（rounds / round_targets / ratings）                    │
│  commands.rs IPC 命令层                                                    │
└──────────────┬────────────────────────────────────────────┬──────────────┘
     desktop 实现│                                 iOS 实现   │
     wry 子 webview ×N                        FamaTabs Swift 插件（WKWebView ×N）
     （Tauri v2 原生支持）                    （或 Tauri 移动端多 webview，M0 定）
```

铁律：

- **UI 永远是我们自己的 HTML**；站点页面只存在于独立的原生 webview，二者绝不混合渲染；
- **平台差异全部封在 `tabs.rs` 的 `WebviewHost` trait 之后**，上层（前端、inject.rs）只面对统一接口；
- **新增站点 = `sites.toml` 一段配置 + 一个注入脚本**，核心代码零改动。

### 1.2 运行时时序（一次「一键多发」）

```
用户在 InputBar 按 Enter
 │
 ▼ invoke('dispatch_question', {question, site_ids, auto_send})
 commands.rs
 ├─► store.rs   INSERT rounds；INSERT round_targets×N (status=dispatching)
 │
 ├─► inject.rs  对每个 site_id 并行（tokio::spawn）：
 │     1. tabs.ensure_loaded(site)            // webview 存在且完成首次导航
 │     2. eval: await famaFill(question)      // 脚本自带 verify
 │     3. auto_send 时 eval: await famaSend()
 │     4. 结果回传（eval 返回值 或 hash-ACK 轮询，≤3s 超时）
 │     5. UPDATE round_targets.status = sent|filled|failed
 │
 ├─► 失败站点降级：clipboard 写入 question + DispatchResult 标记 fallback
 │
 ▼ 返回 DispatchResult[]
前端：TabBar 状态点渲染（✅ 已发送 / 🟡 已填充 / ❌ 失败→点按跳转该站手贴）
```

### 1.3 技术选型与版本基线

| 层 | 选型 | 说明 |
|---|---|---|
| 框架 | Tauri 2.x | 三端（iOS/Win/macOS）系统 webview |
| 前端 | React 18 + Vite + TypeScript | UI 体量小，不引重型状态库 |
| 样式 | 手写 CSS + CSS 变量 | 深浅色主题用 `prefers-color-scheme` |
| 状态 | React Context + `useReducer` | 全局仅 site 列表/激活 tab/发送状态 |
| 存储 | `rusqlite`（SQLite，bundled） | SQL 全部收在 Rust 侧 store.rs，经 IPC 暴露给前端 |
| 剪贴板 | `tauri-plugin-clipboard-manager` | 降级路径 |
| 异步 | tokio（Tauri 自带） | 并行注入 |
| iOS 最低版本 | **iOS 17+** | 为未来独立 cookie 容器（`WKWebsiteDataStore(forIdentifier:)`）留余地 |

## 2. 关键机制详设

### 2.1 布局协商（chrome 与站点 webview 的共存协议）

站点 webview 是**原生子视图**，不在 HTML 文档流内；UI（chrome）是全屏 HTML。二者靠「内容区矩形」协议共存，desktop 与 iOS 插件共用同一套协议：

1. 前端渲染一个透明占位组件 `<ContentArea>`（Tab 栏与输入栏之间）；
2. `ResizeObserver` 监听其 `getBoundingClientRect()`（CSS 逻辑像素），变化时 `invoke('layout_update', { rect, layout })`；
3. `tabs.rs` 把 rect 应用到每个站点 webview 的 frame（逻辑坐标，Tauri/wry 处理 DPR）；
4. 原生 webview 只覆盖内容区 → chrome 区域的触摸/点击落到其下层的 UI webview，两层互不遮挡事件；
5. 切换 Tab = show/hide 对应原生 webview；桌面「分栏」= 前端把 ContentArea 拆成 N 个子 rect，`layout` 参数携带 `[{site_id, rect}]` 数组。

细节：iOS 需叠加 safe-area（`env(safe-area-inset-*)` 计入 rect）；窗口 resize / 旋转 / 键盘弹出都会触发 2（键盘避让见 §9.4）。

### 2.2 WebviewHost 抽象（tabs.rs）

```rust
#[async_trait]
pub trait WebviewHost: Send + Sync {
    async fn ensure_tab(&self, site: &SiteConfig) -> Result<()>;       // 创建+首次导航（幂等）
    fn show(&self, site_id: &str) -> Result<()>;
    fn hide(&self, site_id: &str) -> Result<()>;
    async fn eval_js(&self, site_id: &str, js: &str) -> Result<serde_json::Value>; // 带返回值
    fn apply_layout(&self, items: &[LayoutItem]) -> Result<()>;        // §2.1
    fn on_url_changed(&self, site_id: &str) -> Receiver<Url>;          // hash-ACK 用
}
```

- **desktop 实现**：Tauri v2 子 webview（`WebviewBuilder` 挂到主窗口）；
- **iOS 实现（二选一，M0 定）**：
  - A. Tauri/wry 移动端多 webview 直接可用 → 与 desktop 同路径；
  - B. `FamaTabs` Swift 插件（§9.2），`eval_js` 走原生 `evaluateJavaScript`（带返回值，最可靠）。

### 2.3 注入引擎（inject.rs）

- **脚本组装**：注入器源码（`injectors/*.js`）在构建期打进 assets，运行期读出后包进 IIFE 执行，自包含、不污染页面全局：

```js
(async () => {
  /* injectors/xxx.js 全文 */
  const filled = await famaFill(QUESTION_JSON);   // QUESTION_JSON 由 Rust 注入字符串字面量
  if (!filled.ok)   return { stage: "fill",  ...filled };
  if (!AUTO_SEND)   return { stage: "fill",  ok: true, held: true };
  const sent = await famaSend();
  return { stage: "send", ...sent };
})()
```

- **结果回传双通道**（脚本侧同时满足，Rust 侧择优接收）：
  1. **eval 返回值**：`eval_js` 有返回值时直接用（iOS 插件天然支持；desktop 视 wry 能力）；
  2. **hash-ACK**：脚本入口处先 `location.hash = '#fama-ack=' + encodeURIComponent(JSON.stringify(result))` 再 return。Rust 侧 `on_url_changed` 事件 + 250ms 轮询兜底，**3s 超时**判失败。SPA 站点 hash 变更不触发导航，安全；下次注入会覆写 hash。
- **并发与防重入**：每站点一个 `tokio::spawn` 并行；dispatch 进行中 InputBar 置 disabled（前端 + Rust 侧 `Mutex<()>` 双保险）。
- **超时与重试**：fill 失败自动重试 1 次（间隔 500ms，处理页面未就绪）；send 不重试（防重复发送）。
- **风控自律**：仅响应用户主动触发；无轮询、无后台注入；「仅填充」模式由前端开关控制 `auto_send` 参数。

### 2.4 注入脚本契约与通用模板（injectors/）

每个脚本必须实现两个异步函数（在 IIFE 作用域内声明，Rust 包装器可见）：

```js
async function famaFill(text) { return { ok: true|false, reason?: string } }
async function famaSend()     { return { ok: true|false, reason?: string } }
```

`_generic.js` 通用兜底模板（完整实现，纯 textarea 站点可直接复用）：

```js
// ---- composer 定位 ----
function findComposer() {
  return document.querySelector('form textarea:not([readonly]):not([disabled])')
      || document.querySelector('div textarea:not([readonly]):not([disabled])')
      || document.querySelector('textarea[placeholder]')
      || [...document.querySelectorAll('[contenteditable="true"]')]
           .filter(el => !el.closest('[aria-hidden="true"]'))[0]
      || null;
}
// ---- React 受控 textarea 写值 ----
function setTextareaValue(el, text) {
  const setter = Object.getOwnPropertyDescriptor(
    HTMLTextAreaElement.prototype, 'value').set;
  setter.call(el, text);
  el.dispatchEvent(new Event('input', { bubbles: true }));
}
// ---- contenteditable（ProseMirror 类）写值 ----
function setContentEditable(el, text) {
  el.focus();
  // 优先模拟粘贴（ProseMirror 对 paste 处理最完整）
  const dt = new DataTransfer();
  dt.setData('text/plain', text);
  el.dispatchEvent(new ClipboardEvent('paste', {
    bubbles: true, cancelable: true, clipboardData: dt }));
  if ((el.textContent || '').trim()) return true;
  // 兜底 execCommand
  document.execCommand('insertText', false, text);
  return (el.textContent || '').trim().length > 0;
}
async function famaFill(text) {
  const el = findComposer();
  if (!el) return { ok: false, reason: 'composer-not-found' };
  el.tagName === 'TEXTAREA' ? setTextareaValue(el, text)
                            : setContentEditable(el, text);
  // verify：读回
  const v = el.tagName === 'TEXTAREA' ? el.value : el.textContent;
  return v.includes(text.slice(0, 32)) ? { ok: true } : { ok: false, reason: 'verify-failed' };
}
function findSendButton(composer) {
  const form = composer.closest('form');
  const scope = form || document;
  return scope.querySelector('button[data-testid*="send" i]:not([disabled])')
      || scope.querySelector('button[aria-label*="send" i]:not([disabled])')
      || scope.querySelector('button[aria-label*="发送"]:not([disabled])')
      || scope.querySelector('button[type="submit"]:not([disabled])');
}
async function famaSend() {
  const el = findComposer();
  if (!el) return { ok: false, reason: 'composer-not-found' };
  if (!(el.value || el.textContent || '').trim()) return { ok: false, reason: 'empty' };
  const btn = findSendButton(el);
  if (btn) { btn.click(); return { ok: true, via: 'button' }; }
  el.dispatchEvent(new KeyboardEvent('keydown',
    { key: 'Enter', code: 'Enter', bubbles: true, cancelable: true }));
  return { ok: true, via: 'enter' };
}
```

站点特化脚本（`chatgpt.js` 等）= 复制模板后覆盖 `findComposer` / `findSendButton` 的选择器。**四个首发站点的准确选择器在 M0 真机校准**（下表 M0 填充）：

| 站点 | composer 类型 | 选择器（待校准） | 发送方式（待校准） |
|---|---|---|---|
| chatgpt.com | ProseMirror contenteditable | `#prompt-textarea`（历史值） | 发送按钮 data-testid |
| chatglm.cn | 待校准 | | |
| chat.deepseek.com | 待校准 | | |
| kimi.com | 待校准 | | |

脚本头部带 `// @version N`，注入时随 round_targets 记录，用于排查「哪次发送用了哪个版本的脚本」。

### 2.5 Cookie 容器策略

- v1：**App 级单一容器**（iOS: `WKWebsiteDataStore.default()`；Windows: WebView2 默认数据目录；macOS: wry 默认）。与系统浏览器天然隔离；跨站点 cookie 按域隔离；
- 同站多账号（独立容器）→ 二期：iOS 17 `WKWebsiteDataStore(forIdentifier:)`、WebView2 Profile。

#### 2.5.1 从默认浏览器导入登录 cookie（macOS）

App 与系统浏览器默认互相隔离，新装机后各站点需要重新登录。为此提供**用户主动触发的登录态导入**（`browser_session.rs`，TabBar 🔑 入口）：

- **范围（有意收窄）**：仅支持 Chromium 系默认浏览器（Chrome / Edge / Brave / Vivaldi / Opera / Chromium）。读取其 `"<Browser> Safe Storage"` 钥匙串条目派生解密 key（PBKDF2-HMAC-SHA1 → AES-128-CBC，Chromium v10 格式），以临时副本只读查询 Cookies 数据库，**只导入已配置站点 host 匹配的 cookie**，注入 `WKWebsiteDataStore.defaultDataStore().httpCookieStore`，并 reload 已加载的站点页面。
- **双重授权**：① 前端确认框说明范围与风险；② macOS Keychain ACL 弹窗（系统在 app 首次读取该条目时弹出）。任一拒绝即干净失败，不自动重试；派生 key 缓存在本次进程内存，重复导入不再弹 Keychain。
- **硬前提（macOS TCC / 完全磁盘访问）**：Chromium 系浏览器的用户数据目录受 macOS 隐私保护（TCC）——fama 进程读取 `Local State` / `Cookies` 文件内容会得到 EPERM/EACCES（但 `stat` / `is_file()` 元数据探测不受影响）。因此导入前必须给运行环境授权：「系统设置 → 隐私与安全性 → 完全磁盘访问」——开发模式（`npm run tauri dev`）给运行它的终端 App（如 iTerm2 / Terminal）授权，安装版给 fama.app 授权；授权后完全退出并重启该 App。未授权时导入报 `E_BROWSER_DATA_TCC_DENIED`（含上述指引），一次失败即返回、不重试。
- **多 profile 扫描**：`Local State` 可读时取 `profile.info` 的 key 集合（保证含 `Default`）、`last_used` 优先；不可读（TCC 拒绝）时回退探测已知命名 `Default`、`Profile 1..9`（`is_file()` 元数据探测仍可用）。对所有定位到 Cookies DB 的 profile 逐一副本读取、过滤解密，结果按 (host_key, name) 去重合并（先扫描的 profile 优先；同一 DB 路径只读一次，兼容 Opera 根目录布局）；单个 profile 解密 / 读取失败跳过、不影响其余，全部失败才报错。
- **明确不做**：Safari（TCC / ACL 限制，第三方 WKWebView app 无法稳定读取）、Firefox（无 Safe Storage 机制）、浏览器密码库（Login Data）。检测到这三类默认浏览器直接报 `E_UNSUPPORTED_BROWSER`，不触发 Keychain。
- **数据安全**：Safe Storage 原始 secret 读出即 zeroize；Cookies DB 副本放在临时目录、任何退出路径删除；解密后的 cookie 只注入本 app 自己的 WebKit 数据容器（defaultDataStore，仅本 app 可见），不做中间落盘——但带有效期的 cookie 可能由 WebKit 持久化到本机容器内，与在 app 里正常登录产生的 cookie 行为一致。
- **平台**：Windows（DPAPI）/ Linux（Secret Service）等价能力不在 v1 范围，非 macOS 返回 `E_UNSUPPORTED_PLATFORM`。
- **已知限制**：`NSHTTPCookie` 无法完整表达部分 cookie 属性（HttpOnly / SameSite=None 借助 Foundation 私有属性键尽量保留）；Cookies DB 按候选顺序探测 `{profile}/Cookies` → `{profile}/Network/Cookies` → 数据根 `Cookies` → 数据根 `Network/Cookies`（兼容 Chrome 新版 `Network/` 迁移与 Opera 稳定版无 profile 子目录的布局）；Chromium 未来若更换加密方案（如 v20 app-bound），表现为 skipped 增多并在报告中可见。

## 3. 数据库设计（store.rs）

文件：`{app_config_dir}/fama.db`（rusqlite bundled，WAL 模式）。迁移：`PRAGMA user_version` + 递增迁移数组。

```sql
-- v1
CREATE TABLE rounds (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  question   TEXT    NOT NULL,
  created_at INTEGER NOT NULL              -- unix ms
);

CREATE TABLE round_targets (
  round_id        INTEGER NOT NULL REFERENCES rounds(id) ON DELETE CASCADE,
  site_id         TEXT    NOT NULL,
  status          TEXT    NOT NULL,        -- dispatching|sent|filled|fallback|failed
  injector_ver    TEXT,                    -- 脚本 @version，排查用
  error           TEXT,
  created_at      INTEGER NOT NULL,
  PRIMARY KEY (round_id, site_id)
);
CREATE INDEX idx_rounds_created ON rounds(created_at DESC);

CREATE TABLE ratings (
  round_id   INTEGER NOT NULL,
  site_id    TEXT    NOT NULL,
  rating     INTEGER NOT NULL CHECK (rating IN (1, -1)),
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (round_id, site_id)          -- 同值再点=撤销(删行)
);
```

数据量估算：日均 50 轮 × 4 站 ≈ 200 行/天，单年 <10 万行 —— SQLite 毫无压力，`question` 建全文索引即可支撑未来搜索。

## 4. IPC 契约（commands.rs ↔ src/lib/ipc.ts）

```ts
interface SiteInfo   { id: string; name: string; icon: string }
interface Rect       { x: number; y: number; w: number; h: number }
interface LayoutItem { siteId: string; rect: Rect }          // 单 tab=1 项；分栏=N 项
type  TargetStatus   = 'dispatching' | 'sent' | 'filled' | 'fallback' | 'failed'
interface DispatchResult { siteId: string; status: TargetStatus; error?: string }
interface RoundTarget { siteId: string; status: TargetStatus; rating?: 1 | -1 }
interface Round       { id: number; question: string; createdAt: number; targets: RoundTarget[] }
```

| 命令 | 签名 | 说明 |
|---|---|---|
| `list_sites` | `() → SiteInfo[]` | 启动时拉取 |
| `activate_tab` | `(siteId) → void` | show 当前 + hide 其余（按 layout） |
| `layout_update` | `(items: LayoutItem[]) → void` | §2.1，节流 100ms |
| `dispatch_question` | `(question, siteIds, autoSend) → DispatchResult[]` | 核心命令 |
| `list_rounds` | `(limit=50, before?: createdAt) → Round[]` | 历史分页 |
| `rate` | `(roundId, siteId, rating: 1|-1) → void` | 同值=撤销 |
| `import_browser_session` | `(siteIds) → ImportReport` | 从默认 Chromium 系浏览器导入登录 cookie（§2.5.1）；siteIds 为空=全部站点 |
| `write_clipboard` | `(text) → void` | 降级路径（仅注入失败时调用） |

错误码（invoke reject 的 `code` 字段）：`E_SITES_CONFIG` / `E_TAB_CREATE` / `E_TAB_NOT_READY` / `E_INJECT_TIMEOUT` / `E_INJECT_SCRIPT` / `E_DB`；导入相关：`E_NO_SITES` / `E_DETECT_FAILED` / `E_UNSUPPORTED_BROWSER` / `E_KEYCHAIN_NOT_FOUND` / `E_KEYCHAIN_DENIED` / `E_BROWSER_DATA_MISSING` / `E_BROWSER_DATA_TCC_DENIED` / `E_COOKIE_DB_LOCKED` / `E_DECRYPT_FAILED` / `E_INJECT_FAILED` / `E_UNSUPPORTED_PLATFORM`。

```ts
interface ImportReport {
  browser: string          // 导入来源浏览器名
  profile: string          // 首个扫描的 profile 目录名（兼容字段）
  profilesScanned: string[] // 实际定位到 Cookies 数据库并扫描的全部 profile
  matched: number          // host 匹配到的 cookie 行数（跨 profile 求和）
  injected: number         // 成功注入条数
  skippedDecrypt: number   // 解密失败（非 v10 等）
  skippedOther: number     // 过期 / 非 UTF-8 等
  reloaded: string[]       // 注入后触发 reload 的站点 id
}
```

## 5. 前端设计（src/）

组件树与职责：

```
App
├── TabBar            站点图标 tab；发送后叠加状态点(✅🟡❌)；点按→activate_tab
├── ContentArea       透明占位；ResizeObserver→layout_update（§2.1）
├── InputBar          底部常驻
│   ├── SiteChips     勾选本轮目标站点（记忆上次选择）
│   ├── <textarea>    Enter=发送 / Shift+Enter=换行；dispatch 中 disabled
│   ├── SendButton    与 Enter 等价
│   └── FillOnlyToggle「仅填充」快捷开关（覆盖默认 auto_send）
├── HistorySheet      全屏/侧滑面板：轮次列表(问题+目标+状态)、每目标 👍/👎
└── SettingsSheet     默认发送模式、站点管理（M4：用户自增站点）
```

状态（Context + useReducer）：`sites[]`、`activeTabId`、`selectedSiteIds[]`、`dispatching: boolean`、`lastResults: DispatchResult[]`。

主题：CSS 变量 + `prefers-color-scheme` 双主题；iOS 安全区用 `env(safe-area-inset-*)`；字号跟随系统。

## 6. UI 规格

**iOS**（竖屏主形态）：

```
┌─────────────────────────────┐
│  [GPT] [GLM] [DS] [Kimi]  ⏱ │ Tab + 历史入口；状态点覆盖图标右上
├─────────────────────────────┤
│                             │
│   站点页面（原生 webview）    │ ← ContentArea（safe-area 内）
│                             │
├─────────────────────────────┤
│ (✓GPT ✓GLM ✓DS ✗Kimi)  ⏳   │ SiteChips + 仅填充开关
│ ┌───────────────────┐ [发送]│
│ │ 问点什么…           │      │ Enter=发送
│ └───────────────────┘      │
└─────────────────────────────┘
```

**Desktop**：

```
┌────┬────────────────────────────────┐
│侧 │ Tab栏: [GPT][GLM][DS][Kimi]  ⋮  │
│边 ├────────────────────────────────┤
│栏 │                                │
│   │  站点 webview（或分栏×2/×4）     │
│历 │                                │
│史 ├────────────────────────────────┤
│   │ chips + 输入框 + 发送（同 iOS）  │
└───┴────────────────────────────────┘
```

## 7. 构建与 CI（.github/workflows/）

- `ios.yml`（macOS runner，tag 触发 + 每月例行）：
  `pnpm i → tauri ios build → fastlane pilot upload`（TestFlight）。
  **90 天过期对策**：cron 每月 1 号空 bump build号重传；
- `desktop.yml`（matrix: windows-latest / macos-latest）：`tauri build` 产出 `.msi`/`.exe`、`.dmg`，附到 GitHub Release；
- 本 Linux 环境职责：核心与桌面版开发自测（`tauri dev`）；**iOS 出包必须 macOS**（本机 Xcode 或 CI）。

## 8. 测试策略

| 层 | 方式 |
|---|---|
| Rust 单测 | `sites.toml` 解析/校验；store 迁移与 rate 撤销语义（内存 SQLite） |
| 注入脚本 | 静态检查（导出 famaFill/famaSend）；选择器逻辑在 jsdom 冒烟（不含真实站点） |
| 真机验收 | 每里程碑的手动清单（M0：四站登录+cookie 保持；M1：一键四发+降级路径实测…） |
| 站点改版监控 | 无自动监控；失败率异常时人工更新脚本并 bump @version |

## 9. iOS 工程化

### 9.1 Xcode 工程

`tauri ios init` → `src-tauri/gen/apple/`。签名：Personal Team；bundle id `dev.<待定>.fama`；Info.plist：`UIViewControllerBasedStatusBarAppearance`、键盘避让相关键按 Tauri 模板默认。

### 9.2 FamaTabs Swift 插件（若 M0 判定 wry 路线不可用）

```swift
// Tauri plugin 命令（Rust/前端经 IPC 调用）
@command func tabs_create(id: String, url: URL, rect: Rect) throws
@command func tabs_show(id: String) throws
@command func tabs_hide(id: String) throws
@command func tabs_layout(id: String, rect: Rect) throws   // 逻辑点坐标
@command func tabs_eval(id: String, js: String) async throws -> JSValue
@command func tabs_destroy(id: String) throws
```

- 视图层级：站点 `WKWebView` 作为 `insertSubview(aboveSubview: uiWebview)` 加在 UI webview **之上**，frame 严格等于 ContentArea rect → chrome 区域触摸落到下层 UI webview；
- Cookie：`WKWebsiteDataStore.default()`（App 沙盒持久）；
- `tabs_eval` 用 `evaluateJavaScript`（带返回值 + async 桥），是注入结果最可靠通道。

### 9.3 TestFlight 节奏

首次：Xcode 手动 Archive 上传即可。常态化：CI（§7）。版本策略：`major.minor.patch+build`，例行重传只 bump build。

### 9.4 iOS 细节风险备忘

- 键盘弹出时系统只避让 first responder 所在的 UI webview；**站点 webview 需插件监听 `keyboardWillShow/Hide` 自行缩 frame**（M1 打磨）；
- 个别站点识别 webview UA 引导「打开 App」：必要时插件改 `customUserAgent`（M1 视情况）。

## 10. 里程碑与任务分解

| 里程碑 | 任务 | 验收标准 |
|---|---|---|
| **M0 骨架与风险验证** | ① create-tauri-app（react-ts）+ ios init + Xcode 签名 ② 前端骨架（TabBar/ContentArea/布局上报）③ tabs.rs：desktop 实现先落地（Linux 可测）④ iOS spike-A：Tauri 多 webview 可行性；不可行→spike-B：FamaTabs 插件最小版 ⑤ 四站点登录 + cookie 持久验证 ⑥ spike-C：_generic.js 四站真机跑通 famaFill/famaSend，校准选择器表 | iPhone 上可登录浏览四站；三个 spike 有结论并回填本文档 |
| **M1 核心闭环（iOS）** | dispatch_question 全链路（并行/超时/重试/双通道回传）；TabBar 状态点；剪贴板降级；仅填充开关；防重入；键盘避让 | 一次输入，四站齐发；拔网线/选不存在站点等异常路径可用 |
| **M2 本地记录** | store.rs 三表 + 迁移；HistorySheet（分页）；点赞/踩（同值撤销）；round_targets 状态回填 | 记录重启不丢；评价可加载回放 |
| **M3 桌面端** | wry 子 webview 接入既有 tabs.rs；分栏布局；窗口 resize 布局协商 | Windows/macOS 安装包可用，与 iOS 功能对齐 |
| **M4 打磨与发布** | CI 两条流水线；TestFlight 例行重传；站点配置外部化（用户目录新增 sites.toml 覆盖） | 全平台安装即用；新增站点不动代码 |

## 11. 风险登记册

| # | 风险 | 等级 | 对策 | 兜底 |
|---|---|---|---|---|
| 1 | Tauri v2 iOS 多 webview 不可用 | **高** | M0 spike-A 首先验证 | FamaTabs Swift 插件（§9.2 已完整设计，架构已隔离在 WebviewHost 后） |
| 2 | 站点改版致注入脚本失效 | 中 | 每站独立小脚本；@version 落库可追溯；verify 环节防错发 | hash-ACK 超时→剪贴板降级，功能不断 |
| 3 | desktop eval 无返回值 | 中 | hash-ACK + on_url_changed 轮询 | 同左（已内建） |
| 4 | 站点风控判定自动化 | 中 | 仅用户触发、页面级操作、无轮询；「仅填充」保守模式 | 状态点+人工核对 |
| 5 | TestFlight 90 天过期 | 低 | CI 每月 bump build 重传 | 手动重传 |
| 6 | iOS 键盘/滚动细节 | 低 | 插件监听键盘通知缩 frame（§9.4） | M1 打磨项 |
| 7 | App Store 4.2（若上架） | 低 | TestFlight 自用无审核；上架时以统一输入为差异化定位 | 保持 TestFlight |

## 12. 开放问题（待用户输入）

1. bundle id / App 显示名 / 图标；
2. 四站点注入选择器：M0 真机校准后回填 §2.4 表格；
3. macOS + Xcode 环境确认（iOS 出包前置）；
4. 站点顺序与默认勾选策略（当前假设：全部勾选）。

## 13. 与 feature.md 的映射

| feature.md 条目 | 本设计落点 |
|---|---|
| 2.1 多 Tab 内嵌 | §1.1 / §2.1 / §2.2 / §6 |
| 2.2 统一输入一次多发 | §1.2 / §2.3 / §2.4 / §4 |
| 2.3 本地轻量记录 | §3 / §5（HistorySheet） |
| 2.4 跨端与分发 | §7 / §9 |
| 4. 关键约束与风险 | §2.3 风控自律 / §11 |
