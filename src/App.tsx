import { useCallback, useEffect, useState } from "react";
import TabBar from "./components/TabBar";
import ContentArea from "./components/ContentArea";
import InputBar from "./components/InputBar";
import HistorySheet from "./components/HistorySheet";
import {
  DispatchResult,
  SiteInfo,
  TargetStatus,
  dispatchQuestion,
  listSites,
} from "./lib/ipc";

const STATUS_LABEL: Partial<Record<TargetStatus, string>> = {
  sent: "已发送",
  filled: "已填充（仅填充模式）",
  fallback: "失败，已复制到剪贴板，请切到对应标签手动粘贴",
  failed: "失败",
};

export default function App() {
  const [sites, setSites] = useState<SiteInfo[]>([]);
  const [activeTab, setActiveTab] = useState<string | null>(null);
  const [selected, setSelected] = useState<string[]>([]);
  const [fillOnly, setFillOnly] = useState(false);
  const [dispatching, setDispatching] = useState(false);
  const [statusBySite, setStatusBySite] = useState<
    Record<string, TargetStatus>
  >({});
  const [statusLine, setStatusLine] = useState("");
  const [statusErr, setStatusErr] = useState(false);
  const [historyOpen, setHistoryOpen] = useState(false);

  useEffect(() => {
    void (async () => {
      try {
        const s = await listSites();
        setSites(s);
        if (s.length > 0) {
          setActiveTab(s[0].id);
          setSelected(s.map((x) => x.id)); // 默认全选（design.md §12 待确认项）
        }
      } catch (e) {
        setStatusErr(true);
        setStatusLine(`站点配置加载失败：${e}`);
      }
    })();
  }, []);

  const onSelectTab = useCallback((siteId: string) => {
    setActiveTab(siteId);
  }, []);

  const onToggleSite = useCallback((siteId: string) => {
    setSelected((cur) =>
      cur.includes(siteId)
        ? cur.filter((x) => x !== siteId)
        : [...cur, siteId],
    );
  }, []);

  const onSend = useCallback(
    async (question: string) => {
      setDispatching(true);
      setStatusErr(false);
      setStatusLine("发送中…");
      const targets = selected;
      setStatusBySite((m) => {
        const next = { ...m };
        for (const id of targets) next[id] = "dispatching";
        return next;
      });
      try {
        const results: DispatchResult[] = await dispatchQuestion(
          question,
          targets,
          !fillOnly,
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
    [selected, fillOnly],
  );

  return (
    <div className="app">
      <TabBar
        sites={sites}
        activeId={activeTab}
        statusBySite={statusBySite}
        onSelect={onSelectTab}
        onOpenHistory={() => setHistoryOpen(true)}
      />
      <ContentArea activeSiteId={activeTab} hasSites={sites.length > 0} />
      <InputBar
        sites={sites}
        selected={selected}
        onToggleSite={onToggleSite}
        fillOnly={fillOnly}
        onToggleFillOnly={() => setFillOnly((v) => !v)}
        dispatching={dispatching}
        statusLine={statusLine}
        statusIsError={statusErr}
        onSend={(q) => void onSend(q)}
      />
      {historyOpen && <HistorySheet onClose={() => setHistoryOpen(false)} />}
    </div>
  );
}
