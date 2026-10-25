import { useState } from "react";
import { SiteInfo, importBrowserSession } from "../lib/ipc";
import { errText } from "../lib/importErrors";
import { IS_MACOS } from "../lib/platform";

interface Props {
  sites: SiteInfo[];
  onClose: () => void;
  /** 打开时即展示的错误（启动自动导入失败时由 App 传入；可原地重试） */
  initialError?: string;
}

export default function ImportDialog({ sites, onClose, initialError }: Props) {
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState(initialError ?? "");
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
      // localStorage（kimi 等 SPA 站点）注入数含「立即注入」与「待页面就绪注入」
      const lsN = r.lsInjected + r.lsPending;
      // 读到但既没注入也没挂起（站点页面不可用等）→ 可见提示，不静默丢弃
      const lsDropped = r.lsRead > lsN;
      if (r.injected === 0 && lsN === 0) {
        setSummary(
          `未在 ${r.browser}（profile ${profiles}）找到这些站点的登录数据` +
            (r.matched > 0
              ? `：cookie 匹配到 ${r.matched} 条但均未导入成功。`
              : "（cookie 与 localStorage 均为空）。请先在浏览器里登录对应站点。") +
            (lsDropped
              ? `另有 ${r.lsRead} 条 localStorage 数据未能注入（站点页面暂不可用，重试或切换一次标签页）。`
              : ""),
        );
      } else {
        const skipped =
          r.skippedDecrypt + r.skippedOther > 0
            ? `，跳过 ${r.skippedDecrypt + r.skippedOther} 条（无法解密或已过期）`
            : "";
        const parts = [`${r.injected} 条 cookie`];
        if (lsN > 0)
          parts.push(
            `${lsN} 条 localStorage 登录数据${r.lsPending > 0 ? "（页面就绪后自动注入）" : ""}`,
          );
        setSummary(
          `已从 ${r.browser}（profile ${profiles}）导入 ${parts.join("、")}${skipped}。` +
            (r.reloaded.length > 0
              ? `已刷新：${r.reloaded.join("、")}。`
              : "站点页面将在下次加载时带上登录态。") +
            (lsDropped
              ? `另有 ${r.lsRead - lsN} 条 localStorage 数据未能注入（重试或切换一次标签页）。`
              : ""),
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
              <li>
                只读取已配置站点的登录数据（cookie，及 Kimi
                这类站点的 localStorage 登录令牌），不读取、也不保存浏览器密码；
              </li>
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
