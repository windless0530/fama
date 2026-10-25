import { useCallback, useEffect, useRef, useState } from "react";
import TabBar from "./components/TabBar";
import ContentArea from "./components/ContentArea";
import InputBar from "./components/InputBar";
import HistorySheet from "./components/HistorySheet";
import ImportDialog from "./components/ImportDialog";
import {
  DispatchResult,
  ImportReport,
  LS_RESULT_EVENT,
  LsSiteReport,
  SiteInfo,
  TargetStatus,
  dispatchQuestion,
  importBrowserSessionAuto,
  listSites,
  preloadTabs,
} from "./lib/ipc";
import {
  analyzeImportReport,
  describeLsSite,
  lsSiteProblem,
  mergeLsSites,
} from "./lib/importReport";
import { errCode, errText } from "./lib/importErrors";
import { IS_MACOS } from "./lib/platform";

const STATUS_LABEL: Partial<Record<TargetStatus, string>> = {
  sent: "已发送",
  filled: "已填充（仅填充模式）",
  fallback: "失败，已复制到剪贴板，请切到对应标签手动粘贴",
  failed: "失败",
};

export default function App() {
  const [sites, setSites] = useState<SiteInfo[]>([]);
  const [activeTab, setActiveTab] = useState<string | null>(null);
  const [dispatching, setDispatching] = useState(false);
  const [statusBySite, setStatusBySite] = useState<
    Record<string, TargetStatus>
  >({});
  const [statusLine, setStatusLine] = useState("");
  const [statusErr, setStatusErr] = useState(false);
  const [historyOpen, setHistoryOpen] = useState(false);
  const [importOpen, setImportOpen] = useState(false);
  // 启动自动导入失败时打开 ImportDialog 并展示的错误
  const [importError, setImportError] = useState<string | null>(null);
  // 启动自动导入的报告：打开 ImportDialog 时直接作为结果视图（与手动
  // 导入共用同一分析，cookie 成功不再掩盖 localStorage 失败）
  const [autoReport, setAutoReport] = useState<ImportReport | null>(null);
  // pending localStorage 任务完成的后续事件（按 generation 合并；旧任务
  // 迟到事件不覆盖新结果），实时同步给打开中的 ImportDialog
  const [lsEvents, setLsEvents] = useState<LsSiteReport[]>([]);
  // 事件回调里查站点名（注册时 sites 尚未加载）
  const sitesRef = useRef<SiteInfo[]>([]);
  const nameOf = useCallback(
    (id: string) => sitesRef.current.find((x) => x.id === id)?.name ?? id,
    [],
  );

  // 启动自动复用默认浏览器登录态（macOS；每进程一次，后端守卫）。
  // 分析统一走 analyzeImportReport：存在 localStorage 站点失败/丢失时
  // 即便 cookie 全部成功也打开 ImportDialog 展示站点级结果（不静默）。
  // E_AUTO_ALREADY_RUN = 本进程已导入过（dev 热重载等），静默。
  const autoImport = async (ids: string[]) => {
    try {
      const r = await importBrowserSessionAuto(ids);
      setAutoReport(r);
      setImportError(null);
      const a = analyzeImportReport(r, nameOf);
      setStatusErr(a.statusIsError);
      setStatusLine(a.statusLine);
      if (a.openDialog) setImportOpen(true);
    } catch (e) {
      if (errCode(e) === "E_AUTO_ALREADY_RUN") return;
      setImportError(errText(e));
      setImportOpen(true);
    }
  };

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    void (async () => {
      // 先注册 pending localStorage 结果事件、再触发自动导入，
      // 导入后到位的任务完成事件才不会丢失（StrictMode 双挂载安全）
      if (IS_MACOS) {
        try {
          const { listen } = await import("@tauri-apps/api/event");
          const un = await listen<LsSiteReport>(
            LS_RESULT_EVENT,
            (ev) => {
              const s = ev.payload;
              setLsEvents((prev) => mergeLsSites(prev, [s]));
              if (lsSiteProblem(s)) {
                setStatusErr(true);
                setStatusLine(
                  `部分登录数据未生效：${describeLsSite(s, nameOf)}，详见导入面板`,
                );
                setImportOpen(true);
              } else if (s.injected > 0) {
                setStatusErr(false);
                setStatusLine(describeLsSite(s, nameOf));
              }
            },
          );
          if (cancelled) {
            un();
            return;
          }
          unlisten = un;
        } catch (e) {
          console.error("注册 localStorage 结果事件失败", e);
        }
      }
      try {
        const s = await listSites();
        sitesRef.current = s;
        setSites(s);
        if (s.length > 0) {
          setActiveTab(s[0].id);
          // 预建全部站点 webview（隐藏加载），首次分发/切 tab 即就绪。
          // 与自动导入并发执行：cookie 注入时未完成首载的页面由后端
          // pending_reload 机制在首载完成时补一次刷新，登录态不漏
          void preloadTabs().catch((e) =>
            console.error("preload_tabs 失败", e),
          );
          if (IS_MACOS) {
            void autoImport(s.map((x) => x.id));
          }
        }
      } catch (e) {
        setStatusErr(true);
        setStatusLine(`站点配置加载失败：${e}`);
      }
    })();
    return () => {
      cancelled = true;
      unlisten?.();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const onSelectTab = useCallback((siteId: string) => {
    setActiveTab(siteId);
  }, []);

  const onSend = useCallback(
    async (question: string) => {
      setDispatching(true);
      setStatusErr(false);
      setStatusLine("发送中…");
      // 每题广播全部站点（用户确认的用法），始终自动发送
      const targets = sites.map((s) => s.id);
      setStatusBySite((m) => {
        const next = { ...m };
        for (const id of targets) next[id] = "dispatching";
        return next;
      });
      try {
        const results: DispatchResult[] = await dispatchQuestion(
          question,
          targets,
          true,
        );
        setStatusBySite((m) => {
          const next = { ...m };
          for (const r of results) next[r.siteId] = r.status;
          return next;
        });
        const fails = results.filter(
          (r) => r.status === "fallback" || r.status === "failed",
        );
        setStatusErr(fails.length > 0);
        setStatusLine(
          fails.length > 0
            ? fails
                .map(
                  (f) =>
                    `${f.siteId}: ${STATUS_LABEL[f.status] ?? f.status}${
                      f.error ? `（${f.error}）` : ""
                    }`,
                )
                .join("；")
            : "已全部发送 ✓",
        );
        // 3s 后清掉状态点，避免历史状态残留
        setTimeout(() => {
          setStatusBySite((m) => {
            const next = { ...m };
            for (const id of targets) delete next[id];
            return next;
          });
        }, 3000);
      } catch (e) {
        setStatusErr(true);
        setStatusLine(`发送失败：${e}`);
      } finally {
        setDispatching(false);
      }
    },
    [sites],
  );

  return (
    <div className="app">
      <TabBar
        sites={sites}
        activeId={activeTab}
        statusBySite={statusBySite}
        onSelect={onSelectTab}
        onOpenHistory={() => setHistoryOpen(true)}
        onImportBrowser={() => setImportOpen(true)}
      />
      <ContentArea
        activeSiteId={activeTab}
        hasSites={sites.length > 0}
        onError={(msg) => {
          setStatusErr(true);
          setStatusLine(msg);
        }}
      />
      <InputBar
        dispatching={dispatching}
        statusLine={statusLine}
        statusIsError={statusErr}
        onSend={(q) => void onSend(q)}
      />
      {historyOpen && <HistorySheet onClose={() => setHistoryOpen(false)} />}
      {importOpen && sites.length > 0 && (
        <ImportDialog
          sites={sites}
          initialError={importError ?? undefined}
          initialReport={autoReport ?? undefined}
          lsUpdates={lsEvents}
          onClose={() => {
            setImportOpen(false);
            setImportError(null);
          }}
        />
      )}
    </div>
  );
}
