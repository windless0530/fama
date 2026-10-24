import { useState } from "react";
import { SiteInfo, importBrowserSession } from "../lib/ipc";
import { IS_MACOS } from "../lib/platform";

interface Props {
  sites: SiteInfo[];
  onClose: () => void;
}

/** 后端错误码（"E_XXX: 描述" 前缀）到用户可读提示的映射 */
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

function errText(e: unknown): string {
  const s = String(e);
  const code = s.split(":")[0];
  return ERR_LABEL[code] ?? s;
}

export default function ImportDialog({ sites, onClose }: Props) {
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState("");
  // 成功后的报告；非空时进入"结果"视图
  const [summary, setSummary] = useState("");

  const run = async () => {
    setBusy(true);
    setErr("");
    try {
      const r = await importBrowserSession(sites.map((s) => s.id));
      // 实际扫描到的全部 profile（多 profile 扫描；回退兼容字段 profile）
      const profiles =
        r.profilesScanned.length > 0 ? r.profilesScanned.join("、") : r.profile;
      if (r.injected === 0) {
        setSummary(
          `未在 ${r.browser}（profile ${profiles}）找到这些站点的登录 cookie。` +
            (r.matched > 0
              ? `匹配到 ${r.matched} 条但均未导入成功。`
              : "请先在浏览器里登录对应站点。"),
        );
      } else {
        const skipped =
          r.skippedDecrypt + r.skippedOther > 0
            ? `，跳过 ${r.skippedDecrypt + r.skippedOther} 条（无法解密或已过期）`
            : "";
        setSummary(
          `已从 ${r.browser}（profile ${profiles}）导入 ${r.injected} 条 cookie${skipped}。` +
            (r.reloaded.length > 0
              ? `已刷新：${r.reloaded.join("、")}。`
              : "站点页面将在下次加载时带上登录态。"),
        );
      }
    } catch (e) {
      setErr(errText(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="dialog-overlay" onClick={busy ? undefined : onClose}>
      <div className="dialog-card" onClick={(e) => e.stopPropagation()}>
        <header>
          <span>从浏览器导入登录状态</span>
          {!busy && (
            <button className="close" onClick={onClose}>
              ✕
            </button>
          )}
        </header>
        {summary ? (
          <div className="dialog-body">
            <p>{summary}</p>
            <p className="hint-line">如登录态未生效，可切换一次标签页触发刷新</p>
            <button className="primary" onClick={onClose}>
              完成
            </button>
          </div>
        ) : (
          <div className="dialog-body">
            <p>
              将读取默认浏览器（Chrome / Edge / Brave / Vivaldi / Opera /
              Chromium）中这些站点的登录 cookie 并导入 Fama：
            </p>
            <p className="site-list">{sites.map((s) => s.name).join(" / ")}</p>
            <ul className="notes">
              {IS_MACOS && (
                <li>
                  macOS 会弹出钥匙串授权窗口（"Chrome Safe Storage"），请选择「始终允许」或「允许」；
                </li>
              )}
              {IS_MACOS && (
                <li>
                  浏览器数据受 macOS 隐私保护：需要先在「系统设置 → 隐私与安全性 →
                  完全磁盘访问」里给运行环境授权（开发模式为终端 App，如
                  iTerm2 / Terminal；安装版为 fama.app），否则导入会被系统拦截；
                </li>
              )}
              <li>只读取已配置站点的 cookie，不读取、也不保存浏览器密码；</li>
              <li>
                导入的 cookie 由 Fama 自己的网页数据容器管理，仅本 app
                可见、与浏览器相互独立；带有效期的 cookie 可能被保存在本机。
                如需使其失效，请在 Fama 内打开对应站点并退出登录。
              </li>
            </ul>
            {err && <p className="err-line">{err}</p>}
            <div className="actions">
              <button disabled={busy} onClick={onClose}>
                取消
              </button>
              <button className="primary" disabled={busy} onClick={() => void run()}>
                {busy ? "导入中…（等待钥匙串授权）" : "开始导入"}
              </button>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
