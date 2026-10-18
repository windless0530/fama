// 平台判断，仅用于 UI 展示层（隐藏/调整 macOS 专属入口与文案）；
// 权威判定在后端——非 macOS 的 import_browser_session 返回 E_UNSUPPORTED_PLATFORM。
// macOS WKWebView 的 UA 含 "Macintosh"；iPadOS 桌面级 WebView 会伪装成
// Macintosh，需再用 iPhone/iPad/iPod 排除。

/** 当前是否运行在 macOS（浏览器登录态导入的唯一支持平台） */
export const IS_MACOS =
  /Macintosh|Mac OS X/i.test(navigator.userAgent) &&
  !/iPhone|iPad|iPod/i.test(navigator.userAgent);
