import { useEffect, useRef } from "react";
import { LayoutItem, Rect } from "../lib/ipc";
import { layoutUpdate } from "../lib/ipc";

interface Props {
  activeSiteId: string | null;
  hasSites: boolean;
  onError: (msg: string) => void;
}

/**
 * 透明占位区：站点 webview（原生子视图）按此矩形摆放（design.md §2.1）。
 * ResizeObserver 监听尺寸变化 → 上报 Rust → 调整原生 webview frame。
 */
export default function ContentArea({ activeSiteId, hasSites, onError }: Props) {
  const ref = useRef<HTMLDivElement>(null);
  const lastRect = useRef<Rect | null>(null);
  const lastSiteId = useRef<string | null>(null);
  const rafPending = useRef(false);

  useEffect(() => {
    const el = ref.current;
    if (!el) return;

    const report = () => {
      rafPending.current = false;
      const r = el.getBoundingClientRect();
      const rect: Rect = { x: r.x, y: r.y, w: r.width, h: r.height };
      const last = lastRect.current;
      const siteChanged = lastSiteId.current !== activeSiteId;
      if (
        !siteChanged &&
        last &&
        Math.abs(last.x - rect.x) < 0.5 &&
        Math.abs(last.y - rect.y) < 0.5 &&
        Math.abs(last.w - rect.w) < 0.5 &&
        Math.abs(last.h - rect.h) < 0.5
      ) {
        return;
      }
      lastRect.current = rect;
      lastSiteId.current = activeSiteId;
      if (activeSiteId) {
        layoutUpdate([{ siteId: activeSiteId, rect }]).catch((e) =>
          onError(`布局更新失败（${activeSiteId}）：${e}`),
        );
      }
    };

    const schedule = () => {
      if (rafPending.current) return;
      rafPending.current = true;
      requestAnimationFrame(report);
    };

    const ro = new ResizeObserver(schedule);
    ro.observe(el);
    report();

    // activeSiteId 变化（切 tab / 首次加载）也要上报一次
    schedule();
    return () => ro.disconnect();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeSiteId]);

  return (
    <div className="content-area" ref={ref}>
      <div className="hint">
        <span>{hasSites ? "站点加载中…（可直接在下方输入提问）" : "站点配置加载中…"}</span>
      </div>
    </div>
  );
}

// LayoutItem 在类型上被 layout_update 使用；这里再导出一次避免 tree-shaking 抱怨
export type { LayoutItem };
