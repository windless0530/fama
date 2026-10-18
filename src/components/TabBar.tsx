import { SiteInfo, TargetStatus } from "../lib/ipc";
import { IS_MACOS } from "../lib/platform";

const DOT_CLASS: Partial<Record<TargetStatus, string>> = {
  sent: "sent",
  filled: "held",
  fallback: "fail",
  failed: "fail",
};

interface Props {
  sites: SiteInfo[];
  activeId: string | null;
  statusBySite: Record<string, TargetStatus>;
  onSelect: (siteId: string) => void;
  onOpenHistory: () => void;
  onImportBrowser: () => void;
}

export default function TabBar({
  sites,
  activeId,
  statusBySite,
  onSelect,
  onOpenHistory,
  onImportBrowser,
}: Props) {
  return (
    <div className="tabbar">
      {sites.map((s) => {
        const st = statusBySite[s.id];
        return (
          <button
            key={s.id}
            className={`tab${s.id === activeId ? " active" : ""}`}
            onClick={() => onSelect(s.id)}
          >
            {s.name}
            {st && <span className={`dot ${DOT_CLASS[st] ?? "held"}`} />}
          </button>
        );
      })}
      <span className="spacer" />
      {/* 浏览器登录态导入仅 macOS 支持（后端其他平台返回 E_UNSUPPORTED_PLATFORM） */}
      {IS_MACOS && (
        <button
          className="icon-btn"
          title="从浏览器导入登录状态"
          onClick={onImportBrowser}
        >
          🔑
        </button>
      )}
      <button
        className="icon-btn"
        title="历史记录"
        onClick={onOpenHistory}
      >
        ⏱
      </button>
    </div>
  );
}
