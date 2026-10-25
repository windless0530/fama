// 后端错误码（"E_XXX: 描述" 前缀）到用户可读提示的映射。
// 手动导入（ImportDialog）与启动自动导入（App mount）共用。
// 与 src-tauri/src/browser_session.rs 的错误码保持同步。

const ERR_LABEL: Record<string, string> = {
  E_UNSUPPORTED_BROWSER:
    "默认浏览器不是受支持的 Chromium 系浏览器（支持 Chrome / Edge / Brave / Vivaldi / Opera / Chromium；不支持 Safari 与 Firefox）",
  E_KEYCHAIN_DENIED: "钥匙串授权被拒绝（你在系统弹窗点了「拒绝」或取消了）",
  E_KEYCHAIN_NOT_FOUND:
    "钥匙串中未找到浏览器的 Safe Storage 条目，浏览器可能从未在这台电脑保存过密码",
  E_BROWSER_DATA_MISSING:
    "未找到浏览器的 Cookies 数据库，当前 profile 可能还没在浏览器里登录过任何站点",
  E_BROWSER_DATA_TCC_DENIED:
    "浏览器数据受 macOS 隐私保护（TCC）拦截，fama 无法读取。请按以下步骤授权：「系统设置 → 隐私与安全性 → 完全磁盘访问」：开发模式（npm run tauri dev）给运行它的终端 App（如 iTerm2 / Terminal）开启；安装版给 fama.app 开启。授权后完全退出并重启该 App（终端 / fama.app），再重新导入",
  E_COOKIE_DB_LOCKED:
    "读取浏览器 Cookies 数据库失败，可尝试完全退出浏览器后重试",
  E_DECRYPT_FAILED:
    "Cookies 解密失败，浏览器可能启用了新的加密方案（暂不支持）",
  E_INJECT_FAILED: "Cookies 注入失败",
  E_UNSUPPORTED_PLATFORM: "浏览器登录态导入目前仅支持 macOS",
  E_NO_SITES: "没有可导入的站点",
  E_DETECT_FAILED: "无法识别系统默认浏览器",
};

/** 取错误串里的 "E_XXX" 码 */
export function errCode(e: unknown): string {
  return String(e).split(":")[0];
}

/** 错误串 → 用户可读提示（未知码原样返回） */
export function errText(e: unknown): string {
  const s = String(e);
  return ERR_LABEL[errCode(e)] ?? s;
}
