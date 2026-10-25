import { useCallback, useEffect, useState } from "react";
import TabBar from "./components/TabBar";
import ContentArea from "./components/ContentArea";
import InputBar from "./components/InputBar";
import HistorySheet from "./components/HistorySheet";
import ImportDialog from "./components/ImportDialog";
import {
  DispatchResult,
  SiteInfo,
  TargetStatus,
  dispatchQuestion,
  importBrowserSessionAuto,
  listSites,
  preloadTabs,
} from "./lib/ipc";
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

  // 启动自动复用默认浏览器登录态（macOS；每进程一次，后端守卫）。
  // 成功：状态栏提示；零注入 / 失败：打开 ImportDialog 展示原因，可原地重试
  // （不静默失败）。E_AUTO_ALREADY_RUN = 本进程已导入过（dev 热重载等），静默。
  const autoImport = async (ids: string[]) => {
    try {
      const r = await importBrowserSessionAuto(ids);
      // localStorage（kimi 等）注入数含「立即注入」与「待页面就绪注入」两种
      const lsN = r.lsInjected + r.lsPending;
      if (r.injected > 0 || lsN > 0) {
        const parts = [`${r.injected} 条 cookie`];
        if (lsN > 0) parts.push(`${lsN} 条 localStorage 登录数据`);
        setStatusErr(false);
        setStatusLine(`已复用 ${r.browser} 的登录状态（${parts.join("、")}）`);
      } else if (r.matched > 0) {
        setImportError(
          `浏览器 cookie 匹配到 ${r.matched} 条但均未导入成功（可能启用了新的加密方案），可重试`,
        );
        setImportOpen(true);
      } else {
        setImportError(
          "未在默认浏览器中找到这些站点的登录状态（cookie 与 localStorage 均为空）；可先在浏览器里登录对应站点，再点「开始导入」重试",
        );
        setImportOpen(true);
      }
    } catch (e) {
      if (errCode(e) === "E_AUTO_ALREADY_RUN") return;
      setImportError(errText(e));
      setImportOpen(true);
    }
  };

  useEffect(() => {
    void (async () => {
      try {
        const s = await listSites();
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
          onClose={() => {
            setImportOpen(false);
            setImportError(null);
          }}
        />
      )}
    </div>
  );
}
