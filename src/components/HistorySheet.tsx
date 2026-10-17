import { useEffect, useState } from "react";
import { listRounds, rate, Round } from "../lib/ipc";

interface Props {
  onClose: () => void;
}

const STATUS_LABEL: Record<string, string> = {
  sent: "已发送",
  filled: "已填充",
  fallback: "降级",
  failed: "失败",
  dispatching: "发送中",
};

export default function HistorySheet({ onClose }: Props) {
  const [rounds, setRounds] = useState<Round[]>([]);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    void (async () => {
      try {
        setRounds(await listRounds(100));
      } finally {
        setLoading(false);
      }
    })();
  }, []);

  const onRate = async (roundId: number, siteId: string, v: 1 | -1) => {
    // 同值再点 = 撤销（后端语义），前端直接镜像更新
    const cur = rounds
      .find((r) => r.id === roundId)
      ?.targets.find((t) => t.siteId === siteId)?.rating;
    const next = cur === v ? undefined : v;
    setRounds((rs) =>
      rs.map((r) =>
        r.id !== roundId
          ? r
          : {
              ...r,
              targets: r.targets.map((t) =>
                t.siteId === siteId ? { ...t, rating: next } : t,
              ),
            },
      ),
    );
    try {
      await rate(roundId, siteId, v);
    } catch (e) {
      console.error("rate failed", e);
    }
  };

  return (
    <div className="history">
      <header>
        <span>历史轮次</span>
        <button className="close" onClick={onClose}>
          ✕
        </button>
      </header>
      <div className="list">
        {loading && <div>加载中…</div>}
        {!loading && rounds.length === 0 && <div>还没有记录</div>}
        {rounds.map((r) => (
          <div className="round-card" key={r.id}>
            <div className="time">
              {new Date(r.createdAt).toLocaleString()}
            </div>
            <div className="q">{r.question}</div>
            <div className="targets">
              {r.targets.map((t) => (
                <span
                  className="target"
                  key={t.siteId}
                  data-rated={t.rating ?? 0}
                >
                  {t.siteId}
                  <span className={`status ${t.status}`}>
                    {STATUS_LABEL[t.status] ?? t.status}
                  </span>
                  <button
                    className="rate up"
                    title="赞"
                    onClick={() => void onRate(r.id, t.siteId, 1)}
                  >
                    👍
                  </button>
                  <button
                    className="rate down"
                    title="踩"
                    onClick={() => void onRate(r.id, t.siteId, -1)}
                  >
                    👎
                  </button>
                </span>
              ))}
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}
