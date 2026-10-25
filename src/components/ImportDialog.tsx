import { useEffect, useState } from "react";
import {
  ImportReport,
  LsSiteReport,
  SiteInfo,
  importBrowserSession,
} from "../lib/ipc";
import {
  analyzeImportReport,
  describeLsSite,
  lsSiteProblem,
  mergeLsSites,
} from "../lib/importReport";
import { errText } from "../lib/importErrors";
import { IS_MACOS } from "../lib/platform";

interface Props {
  sites: SiteInfo[];
  onClose: () => void;
  /** 打开时即展示的错误（启动自动导入失败时由 App 传入；可原地重试） */
  initialError?: string;
  /** 启动自动导入的报告：非空时直接进入结果视图（与手动导入同一分析） */
  initialReport?: ImportReport;
  /** pending localStorage 任务的后续结果事件（按 generation 合并展示） */
  lsUpdates?: LsSiteReport[];
}

export default function ImportDialog({
  sites,
  onClose,
  initialError,
  initialReport,
  lsUpdates,
}: Props) {
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState(initialError ?? "");
  // 已有导入报告时直接进入"结果"视图（自动导入的初始报告或手动导入结果）
  const [report, setReport] = useState<ImportReport | null>(
    initialReport ?? null,
  );
  // 后续事件按 generation 合并后的站点级实时状态
  const [liveSites, setLiveSites] = useState<LsSiteReport[]>([]);

  // App 侧状态变化时同步内部视图（新的自动导入结果 / 错误）
  useEffect(() => {
    setErr(initialError ?? "");
  }, [initialError]);
  useEffect(() => {
    setReport(initialReport ?? null);
  }, [initialReport]);
  useEffect(() => {
    setLiveSites((prev) => mergeLsSites(prev, lsUpdates ?? []));
  }, [lsUpdates]);

  const nameOf = (id: string) =>
    sites.find((s) => s.id === id)?.name ?? id;

  const run = async () => {
    setBusy(true);
    setErr("");
    try {
      const r = await importBrowserSession(sites.map((s) => s.id));
      setReport(r);
    } catch (e) {
      setErr(errText(e));
    } finally {
      setBusy(false);
    }
  };

  // 统一分析：cookie 成功但 localStorage 站点失败/丢失时不再显示纯成功
  const analysis = report ? analyzeImportReport(report, nameOf) : null;
  // 结果视图里的站点级明细：报告快照 + 后续事件按 generation 合并
  const lsLines = report
    ? mergeLsSites(report.lsSites, liveSites)
    : mergeLsSites([], liveSites);
  const canRetry =
    (analysis?.canRetry ?? false) || liveSites.some(lsSiteProblem);
  const profiles = report
    ? report.profilesScanned.length > 0
      ? report.profilesScanned.join("、")
      : report.profile
    : null;

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
        {analysis && report ? (
          <div className="dialog-body">
            <p>{analysis.dialogSummary}</p>
            {profiles && (
              <p className="hint-line">
                来源：{report.browser}（profile {profiles}）
                {report.reloaded.length > 0
                  ? `；已刷新：${report.reloaded.map(nameOf).join("、")}`
                  : ""}
              </p>
            )}
            {lsLines.length > 0 && (
              <ul className="ls-sites">
                {lsLines.map((s) => (
                  <li
                    key={s.siteId}
                    className={lsSiteProblem(s) ? "bad" : undefined}
                  >
                    {describeLsSite(s, nameOf)}
                  </li>
                ))}
              </ul>
            )}
            <p className="hint-line">
              如登录态未生效，可切换一次标签页触发刷新；待页面就绪的注入会自动完成并更新
            </p>
            <div className="actions">
              {canRetry && (
                <button disabled={busy} onClick={() => void run()}>
                  {busy ? "导入中…" : "重试导入"}
                </button>
              )}
              <button className="primary" disabled={busy} onClick={onClose}>
                完成
              </button>
            </div>
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
