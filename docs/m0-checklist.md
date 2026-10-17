# M0 真机验证清单（iOS / Mac 侧操作指南）

> 本 Linux 环境已完成：工程骨架、Rust 核心、前端 UI、注入脚本模板、desktop 路径代码。
> 以下三项 spike 必须在 **macOS + iPhone** 上完成，结论回填 design.md。

## 前置准备

1. 一台 macOS（Xcode 15+）+ iPhone（数据线配对）；
2. Apple 开发者账号（个人版 $99/年）已在 Xcode 登录；
3. 把本仓库拉到 Mac：`git clone <repo> && cd fama && npm install`；
4. 安装 Rust（如未装）：`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`；
5. 安装 Xcode 命令行工具与 iOS 目标：`rustup target add aarch64-apple-ios`。

## 初始化 iOS 工程（一次性）

```bash
npm run tauri ios init        # 生成 src-tauri/gen/apple
```

然后打开 `src-tauri/gen/apple` 下的 Xcode 工程：

- Signing & Capabilities → Team 选你的个人 Team；
- Bundle Identifier 改成你自己的（如 `dev.你的名字.fama`）——design.md §12 开放问题；
- 连接 iPhone，选择其为运行目标，Run。

## Spike A：Tauri 移动端多 webview 可行性（最高优先级）

**问题**：Tauri v2 在 iOS 上能否创建多个子 webview（我们的站点 Tab 依赖它）？

**步骤**：
1. 先在 iOS 模拟器/真机跑通当前工程（UI 是 HTML chrome + 空内容区）；
2. 观察 `layout_update` → `tabs::ensure_tab` 是否在 iOS 上成功创建 webview
   （切 Tab 时内容区出现站点页面 = 可行）；
3. 若报错/空白/崩溃 → 记录错误信息。

**判定**：
- ✅ 可行 → desktop 与 iOS 共用 wry 路径，M1 直接开始；
- ❌ 不可行 → 启用 FamaTabs Swift 插件（design.md §9.2 的 API 设计照抄实现），
  只改 `tabs.rs` 的 iOS 分支。

## Spike B：hash-ACK 通道在 iOS 上的表现

**问题**：站点 webview 改 `location.hash` 后，`webview.url()` 能否读到 fragment？

**步骤**：真机登录任一站点后，在输入框发一个问题：
- 状态点变 ✅/🟡 → 双通道正常；
- 3s 超时变 ❌ 且带「结果回传超时」→ iOS 上 eval 无返回值且 hash 读不到，
  FamaTabs 插件需带 `evaluateJavaScript` 返回值通道（它会天然解决）。

## Spike C：四站点注入脚本校准

对 ChatGPT / GLM / DeepSeek / Kimi 逐站：

1. 在 App 内登录该站；
2. 仅勾选该站，发送测试问题（先用「仅填充」模式，确认填入正确后再开自动发送）；
3. 失败时在 Mac 上用 Safari 远程调试（iPhone：设置→Safari→高级→Web 检查器；
   Mac Safari→开发菜单→选 iPhone）查看 DOM，找到真实选择器；
4. 更新 `src-tauri/injectors/<site>.js` 的 `findComposer` / `findSendButton`，
   `// @version` +1；
5. 回填 design.md §2.4 的选择器表格。

## 完成标志

- [ ] Spike A 有结论（wry 多 webview 可用 / 需 FamaTabs 插件）
- [ ] Spike B 有结论（hash-ACK 可用 / 需 eval 返回值通道）
- [ ] 四站点「仅填充」可用
- [ ] 四站点「自动发送」可用
- [ ] cookie 登录态在 App 重启后保持
- [ ] 结论回填 design.md
