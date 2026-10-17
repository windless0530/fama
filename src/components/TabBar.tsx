import { SiteInfo, TargetStatus } from "../lib/ipc";

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
}

export default function TabBar({
  sites,
  activeId,
  statusBySite,
  onSelect,
  onOpenHistory,
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
