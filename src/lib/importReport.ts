// 自动导入（App.tsx）与手动导入（ImportDialog.tsx）共用的报告分析。
//
// 核心规则（与后端 tabs.rs LsSiteReport 契约一致）：
// - lsInjected 只含来自合法 ACK 的确认条数；eval 派发成功不计入。
// - pending 是「等待页面就绪」，不是成功，不得描述为已注入。
// - 单站失败：failed > 0 或 read > injected + pending（读到但未注入也未挂起）。
// - 任一 localStorage 站点失败都不得被 cookie 成功或其他站点的聚合计数掩盖。

import type { ImportReport, LsSiteReport } from "./ipc";

/** 站点名查找（siteId → 用户可见名；缺省回退 siteId） */
export type SiteNameOf = (siteId: string) => string;

/** 单个 localStorage 站点是否存在需要用户处理的问题 */
export function lsSiteProblem(s: LsSiteReport): boolean {
  return s.failed > 0 || s.read > s.injected + s.pending;
}

/**
 * 按站点合并两批 LsSiteReport：siteId 相同者取 generation 更新的；
 * generation 相同取 incoming（后到的更新覆盖）。旧任务的迟到事件
 * 因此无法覆盖新任务的结果。
 */
export function mergeLsSites(
  current: LsSiteReport[],
  incoming: LsSiteReport[],
): LsSiteReport[] {
  const map = new Map(current.map((s) => [s.siteId, s]));
  for (const s of incoming) {
    const prev = map.get(s.siteId);
    if (!prev || s.generation >= prev.generation) map.set(s.siteId, s);
  }
  return [...map.values()].sort((a, b) => a.siteId.localeCompare(b.siteId));
}

/** 单站点的一行人类可读状态（不含任何 key/value） */
export function describeLsSite(s: LsSiteReport, nameOf: SiteNameOf): string {
  const name = nameOf(s.siteId);
  // 未注入且未挂起的条数里，failed 已单列，其余才是「丢失」
  const droppedOther = Math.max(
    0,
    s.read - s.injected - s.pending - s.failed,
  );
  const pendingNote =
    s.pending > 0 && (s.injected > 0 || s.failed > 0 || droppedOther > 0)
      ? `，另 ${s.pending} 条待页面就绪`
      : "";
  const errNote = s.error ? `（${s.error}）` : "";
  if (s.read === 0) return `${name}：浏览器中无该站点的登录数据`;
  if (s.injected === 0 && s.pending > 0 && s.failed === 0) {
    return `${name}：${s.pending} 条登录数据待页面就绪后自动注入`;
  }
  if (s.failed > 0 || droppedOther > 0) {
    return (
      `${name}：已注入 ${s.injected}/${s.read} 条` +
      (s.failed > 0 ? `，${s.failed} 条写入失败` : "") +
      (s.failed > 0 && droppedOther > 0 ? "、" : "") +
      (droppedOther > 0 ? `${droppedOther} 条未注入` : "") +
      pendingNote +
      errNote
    );
  }
  return `${name}：${s.injected} 条登录数据已确认注入${pendingNote}`;
}

/** analyzeImportReport 的输出（statusLine 给状态栏；dialogSummary 给导入面板） */
export interface ImportAnalysis {
  /** true = 需要用户处理：App 应打开（或保持）ImportDialog */
  openDialog: boolean;
  /** 状态栏是否按错误样式显示 */
  statusIsError: boolean;
  /** 状态栏一行摘要 */
  statusLine: string;
  /** ImportDialog 结果视图的完整文字（含站点级明细） */
  dialogSummary: string;
  /** 结果视图是否提供「重试导入」 */
  canRetry: boolean;
}

/**
 * 统一分析导入报告。自动导入与手动导入共用，保证 cookie 成功 +
 * localStorage 失败的场景一定可见（不再被成功分支掩盖）。
 */
export function analyzeImportReport(
  r: ImportReport,
  nameOf: SiteNameOf = (id) => id,
): ImportAnalysis {
  const lsWaiting = r.lsInjected + r.lsPending;
  const problems = r.lsSites.filter(lsSiteProblem);
  // lsSites 为空（旧后端 / 非 localStorage 站点）时退回聚合判定
  const hasFailure =
    r.lsFailed > 0 || r.lsRead > lsWaiting || problems.length > 0;

  // ---- 完全没有导入成功的条目：沿用「未找到 / 均未导入」文案 ----
  if (r.injected === 0 && lsWaiting === 0) {
    if (r.matched > 0) {
      return {
        openDialog: true,
        statusIsError: true,
        statusLine: "浏览器 cookie 均未导入成功，详见导入面板",
        dialogSummary:
          `浏览器 cookie 匹配到 ${r.matched} 条但均未导入成功` +
          "（可能启用了新的加密方案），可重试",
        canRetry: true,
      };
    }
    return {
      openDialog: true,
      statusIsError: true,
      statusLine: "未找到可复用的浏览器登录状态，详见导入面板",
      dialogSummary:
        "未在默认浏览器中找到这些站点的登录状态（cookie 与 localStorage 均为空）；" +
        "可先在浏览器里登录对应站点，再点「开始导入」重试",
      canRetry: true,
    };
  }

  const parts = [`${r.injected} 条 cookie`];
  if (lsWaiting > 0) {
    parts.push(
      `${r.lsInjected} 条 localStorage 登录数据` +
        (r.lsPending > 0 ? `（另 ${r.lsPending} 条待页面就绪）` : ""),
    );
  }

  // ---- 有失败 / 丢失：部分成功，必须可见（即使 cookie 全部成功）----
  if (hasFailure) {
    const siteLines = problems.map((s) => describeLsSite(s, nameOf));
    const aggregate =
      problems.length === 0
        ? `${Math.max(0, r.lsRead - lsWaiting)} 条 localStorage 数据未能注入`
        : "";
    const detail = [...siteLines, aggregate].filter(Boolean).join("；");
    return {
      openDialog: true,
      statusIsError: true,
      statusLine: `已复用部分登录状态（${parts.join("、")}）；${detail}，详见导入面板`,
      dialogSummary:
        `已从 ${r.browser} 导入 ${parts.join("、")}，但部分登录数据未生效：` +
        `${detail}。` +
        "可点「重试导入」重试；待页面就绪的部分会自动完成并更新结果。",
      canRetry: true,
    };
  }

  // ---- 全部成功 ----
  return {
    openDialog: false,
    statusIsError: false,
    statusLine: `已复用 ${r.browser} 的登录状态（${parts.join("、")}）`,
    dialogSummary: `已从 ${r.browser} 导入 ${parts.join("、")}。`,
    canRetry: false,
  };
}
