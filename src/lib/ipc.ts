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

// 浏览器登录态导入报告（commands.rs import_browser_session 的契约）
export interface ImportReport {
  browser: string;
  profile: string;
  matched: number;
  injected: number;
  skippedDecrypt: number;
  skippedOther: number;
  reloaded: string[];
}

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

/** 从默认 Chromium 系浏览器导入登录 cookie；siteIds 为空 = 全部站点 */
export async function importBrowserSession(
  siteIds: string[],
): Promise<ImportReport> {
  return invoke("import_browser_session", { siteIds });
}
