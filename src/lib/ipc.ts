import { invoke } from "@tauri-apps/api/core";

// 与 src-tauri/src/commands.rs 的契约保持同步（design.md §4）

export interface SiteInfo {
  id: string;
  name: string;
  icon: string;
}

export interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}

export interface LayoutItem {
  siteId: string;
  rect: Rect;
}

export type TargetStatus =
  | "dispatching"
  | "sent"
  | "filled"
  | "fallback"
  | "failed";

export interface DispatchResult {
  siteId: string;
  status: TargetStatus;
  error?: string;
}

export interface RoundTarget {
  siteId: string;
  status: TargetStatus;
  rating?: 1 | -1;
}

export interface Round {
  id: number;
  question: string;
  createdAt: number;
  targets: RoundTarget[];
}

/** localStorage 注入的站点级结果（tabs.rs LsSiteReport 的契约） */
export interface LsSiteReport {
  siteId: string;
  /** 任务 generation；越大越新，事件合并时旧 generation 不得覆盖新结果 */
  generation: number;
  /** 从浏览器 LevelDB 读到的条目数 */
  read: number;
  /** 来自合法 ACK 的确认注入条数 */
  injected: number;
  /** 仍在等待页面就绪的任务条数（未确认，不得显示为成功） */
  pending: number;
  /** ACK 报告的写入失败条数 */
  failed: number;
  status: "ok" | "partial" | "failed" | "pending";
  /** 固定错误类别文案；不含任何 key/value */
  error?: string;
}

// 浏览器登录态导入报告（commands.rs import_browser_session 的契约）
export interface ImportReport {
  browser: string;
  /** 首个扫描的 profile（兼容字段；完整列表见 profilesScanned） */
  profile: string;
  /** 实际定位到 Cookies 数据库并扫描的全部 profile 目录名 */
  profilesScanned: string[];
  matched: number;
  injected: number;
  skippedDecrypt: number;
  skippedOther: number;
  reloaded: string[];
  /** localStorage 扫描读到的条目数（kimi 等不走 cookie 认证的站点） */
  lsRead: number;
  /** 来自合法 ACK 的确认注入条数（eval 派发成功不计入） */
  lsInjected: number;
  /** 挂起待站点回到配置 origin 后注入的 localStorage 条数 */
  lsPending: number;
  /** 站点级写入失败条数（lsSites 求和；cookie 成功不能抵消） */
  lsFailed: number;
  /** localStorage 注入的站点级明细 */
  lsSites: LsSiteReport[];
}

/** pending localStorage 任务完成时由后台 worker 发出的 Tauri 事件 */
export const LS_RESULT_EVENT = "browser-import-ls-result";

export async function listSites(): Promise<SiteInfo[]> {
  return invoke("list_sites");
}

export async function preloadTabs(): Promise<void> {
  await invoke("preload_tabs");
}

export async function layoutUpdate(items: LayoutItem[]): Promise<void> {
  await invoke("layout_update", { items });
}

export async function activateTab(siteId: string): Promise<void> {
  await invoke("activate_tab", { siteId });
}

export async function dispatchQuestion(
  question: string,
  siteIds: string[],
  autoSend: boolean,
): Promise<DispatchResult[]> {
  return invoke("dispatch_question", { question, siteIds, autoSend });
}

export async function listRounds(
  limit = 50,
  before?: number,
): Promise<Round[]> {
  return invoke("list_rounds", { limit, before });
}

export async function rate(
  roundId: number,
  siteId: string,
  rating: 1 | -1,
): Promise<void> {
  await invoke("rate", { roundId, siteId, rating });
}

/** 从默认 Chromium 系浏览器导入登录 cookie（手动入口）；siteIds 为空 = 全部站点 */
export async function importBrowserSession(
  siteIds: string[],
): Promise<ImportReport> {
  return invoke("import_browser_session", { siteIds });
}

/**
 * 启动时自动导入登录 cookie（每进程至多一次；重复调用 reject
 * "E_AUTO_ALREADY_RUN"，调用方应静默忽略）。macOS 首次会在读取浏览器
 * Safe Storage 时弹 Keychain 授权框——那既是授权点也是用户感知点。
 */
export async function importBrowserSessionAuto(
  siteIds: string[],
): Promise<ImportReport> {
  return invoke("import_browser_session_auto", { siteIds });
}
