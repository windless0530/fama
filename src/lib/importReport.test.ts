// analyzeImportReport / mergeLsSites / describeLsSite 的纯函数测试。
// 对应终审 #3：自动导入的成功分支不得掩盖 localStorage 完全未注入。
import { describe, expect, it } from "vitest";
import {
  analyzeImportReport,
  describeLsSite,
  lsSiteProblem,
  mergeLsSites,
} from "./importReport";
import type { ImportReport, LsSiteReport } from "./ipc";

const NAMES = (id: string) => (id === "kimi" ? "Kimi" : id);

function lsSite(p: Partial<LsSiteReport>): LsSiteReport {
  return {
    siteId: "kimi",
    generation: 1,
    read: 0,
    injected: 0,
    pending: 0,
    failed: 0,
    status: "ok",
    ...p,
  };
}

function report(p: Partial<ImportReport>): ImportReport {
  return {
    browser: "Chrome",
    profile: "Default",
    profilesScanned: ["Default"],
    matched: 0,
    injected: 0,
    skippedDecrypt: 0,
    skippedOther: 0,
    reloaded: [],
    lsRead: 0,
    lsInjected: 0,
    lsPending: 0,
    lsFailed: 0,
    lsSites: [],
    ...p,
  };
}

describe("analyzeImportReport", () => {
  it("cookie 全部成功但 kimi localStorage 完全未注入时必须警告并打开面板", () => {
    // 终审 #3 的原始场景：cookie 53 条成功 + kimi 读到 65 条注入 0
    const r = report({
      matched: 53,
      injected: 53,
      lsSites: [lsSite({ read: 65, injected: 0, pending: 0 })],
      lsRead: 65,
      lsInjected: 0,
      lsPending: 0,
    });
    const a = analyzeImportReport(r, NAMES);
    expect(a.openDialog).toBe(true);
    expect(a.statusIsError).toBe(true);
    expect(a.statusLine).toContain("Kimi");
    expect(a.dialogSummary).toContain("65");
    expect(a.canRetry).toBe(true);
  });

  it("cookie 成功 + kimi 部分失败 → 部分成功（可见失败明细）", () => {
    const r = report({
      injected: 53,
      lsSites: [lsSite({ read: 65, injected: 60, failed: 5, status: "partial" })],
      lsRead: 65,
      lsInjected: 60,
      lsFailed: 5,
    });
    const a = analyzeImportReport(r, NAMES);
    expect(a.openDialog).toBe(true);
    expect(a.statusIsError).toBe(true);
    expect(a.statusLine).toContain("已复用部分登录状态");
    expect(a.dialogSummary).toContain("5 条写入失败");
  });

  it("多站聚合不能掩盖单站失败：其他站成功也要点名失败站", () => {
    const r = report({
      injected: 40,
      lsSites: [
        lsSite({ siteId: "chatgpt", read: 30, injected: 30 }),
        lsSite({ read: 65, injected: 65, failed: 0, pending: 0 }),
        lsSite({ siteId: "deepseek", read: 20, injected: 0, failed: 20, status: "failed" }),
      ],
      lsRead: 115,
      lsInjected: 95,
      lsFailed: 20,
    });
    const a = analyzeImportReport(r, NAMES);
    expect(a.openDialog).toBe(true);
    expect(a.dialogSummary).toContain("deepseek");
    expect(a.dialogSummary).toContain("20 条写入失败");
  });

  it("pending 只显示为待页面就绪，不得计为已确认注入", () => {
    const r = report({
      injected: 53,
      lsSites: [lsSite({ read: 65, injected: 0, pending: 65, status: "pending" })],
      lsRead: 65,
      lsPending: 65,
    });
    const a = analyzeImportReport(r, NAMES);
    // 等待不是失败：不弹错误
    expect(a.openDialog).toBe(false);
    expect(a.statusIsError).toBe(false);
    expect(a.statusLine).toContain("0 条 localStorage 登录数据");
    expect(a.statusLine).toContain("另 65 条待页面就绪");
    expect(a.statusLine).not.toContain("已复用 Chrome 的登录状态（53 条 cookie、65 条");
    const line = describeLsSite(r.lsSites[0], NAMES);
    expect(line).toContain("待页面就绪");
    expect(line).not.toContain("已确认注入");
  });

  it("全部成功时不打开面板，状态行无错误", () => {
    const r = report({
      injected: 53,
      lsSites: [lsSite({ read: 65, injected: 65 })],
      lsRead: 65,
      lsInjected: 65,
    });
    const a = analyzeImportReport(r, NAMES);
    expect(a.openDialog).toBe(false);
    expect(a.statusIsError).toBe(false);
    expect(a.statusLine).toContain("53 条 cookie");
    expect(a.statusLine).toContain("65 条 localStorage 登录数据");
  });

  it("零导入 + cookie 匹配失败 → 打开面板可重试", () => {
    const a = analyzeImportReport(report({ matched: 10 }), NAMES);
    expect(a.openDialog).toBe(true);
    expect(a.canRetry).toBe(true);
    expect(a.dialogSummary).toContain("10 条");
  });

  it("零导入且无匹配 → 提示先登录", () => {
    const a = analyzeImportReport(report({}), NAMES);
    expect(a.openDialog).toBe(true);
    expect(a.dialogSummary).toContain("登录");
  });

  it("lsSites 为空时退回聚合判定（读到未注入仍可见）", () => {
    const r = report({ injected: 53, lsRead: 65, lsInjected: 0, lsPending: 0 });
    const a = analyzeImportReport(r, NAMES);
    expect(a.openDialog).toBe(true);
    expect(a.statusIsError).toBe(true);
    expect(a.dialogSummary).toContain("65 条");
  });
});

describe("lsSiteProblem", () => {
  it("failed > 0 或 read > injected + pending 判定为问题", () => {
    expect(lsSiteProblem(lsSite({ read: 10, injected: 10 }))).toBe(false);
    expect(lsSiteProblem(lsSite({ read: 10, injected: 0, pending: 10 }))).toBe(false);
    expect(lsSiteProblem(lsSite({ read: 10, injected: 8, failed: 2 }))).toBe(true);
    expect(lsSiteProblem(lsSite({ read: 10, injected: 5, pending: 0 }))).toBe(true);
    expect(lsSiteProblem(lsSite({ read: 10, injected: 5, pending: 4 }))).toBe(true);
  });
});

describe("mergeLsSites", () => {
  it("旧 generation 的迟到事件不能覆盖新结果", () => {
    const cur = [lsSite({ generation: 5, read: 65, injected: 65 })];
    const late = [lsSite({ generation: 3, read: 65, injected: 0, failed: 65, status: "failed" })];
    const merged = mergeLsSites(cur, late);
    expect(merged).toHaveLength(1);
    expect(merged[0].generation).toBe(5);
    expect(lsSiteProblem(merged[0])).toBe(false);
  });

  it("更高 generation 的后续失败事件覆盖旧的成功（面板随之更新）", () => {
    // 支撑「后续失败能更新已打开的 ImportDialog」的合并语义
    const cur = [lsSite({ generation: 5, read: 65, injected: 65 })];
    const next = [lsSite({ generation: 6, read: 65, injected: 0, failed: 65, status: "failed" })];
    const merged = mergeLsSites(cur, next);
    expect(merged[0].generation).toBe(6);
    expect(lsSiteProblem(merged[0])).toBe(true);
  });

  it("同 generation 时后到者胜（更新覆盖快照）", () => {
    const cur = [lsSite({ generation: 5, status: "pending", pending: 65, read: 65 })];
    const next = [lsSite({ generation: 5, read: 65, injected: 65 })];
    const merged = mergeLsSites(cur, next);
    expect(merged[0].injected).toBe(65);
    expect(merged[0].pending).toBe(0);
  });

  it("不同站点互不覆盖，输出按 siteId 排序", () => {
    const merged = mergeLsSites(
      [lsSite({ siteId: "kimi", read: 1, injected: 1 })],
      [lsSite({ siteId: "chatgpt", read: 2, injected: 2 })],
    );
    expect(merged.map((s) => s.siteId)).toEqual(["chatgpt", "kimi"]);
  });
});
