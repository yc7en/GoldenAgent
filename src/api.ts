/**
 * 与壳层通信的唯一出口。
 *
 * 这一层只做两件事：给每个 command 一个类型，以及把后端的字符串错误
 * 统一成 `Error`。散在各处的 `String(error)` 会让「错误长什么样」变成
 * 每个组件各自决定的事——那正是错误提示不一致的根源。
 */

import { invoke } from "@tauri-apps/api/core";

import type {
  AppInfo,
  ChatReport,
  DeleteReport,
  Message,
  ProviderSetup,
  SearchHit,
  SessionHits,
  SessionSummary,
  Snapshot,
  SpaceInfo,
} from "./types";

/** Tauri 的 `Err(String)` 到这里还是字符串，转成 Error 才能带栈。 */
export function toError(error: unknown): Error {
  if (error instanceof Error) return error;
  if (typeof error === "string") return new Error(error);
  try {
    return new Error(JSON.stringify(error));
  } catch {
    return new Error(String(error));
  }
}

async function call<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  try {
    return await invoke<T>(command, args);
  } catch (error) {
    throw toError(error);
  }
}

export const api = {
  snapshot: () => call<Snapshot>("snapshot"),

  modes: () => call<{ current: string; all: string[] }>("modes"),
  setMode: (mode: string) => call<string>("set_mode", { mode }),

  chat: (prompt: string) => call<ChatReport>("chat", { prompt }),
  cancel: () => call<boolean>("cancel"),
  history: () => call<Message[]>("history"),
  resolvePermission: (id: string, allowed: boolean) =>
    call<boolean>("resolve_permission", { id, allowed }),

  // 会话
  listSessions: (spaceId: string | null, includeArchived: boolean) =>
    call<SessionSummary[]>("list_sessions", {
      spaceId,
      includeArchived,
    }),
  openSession: (id: string) => call<Message[]>("open_session", { id }),
  newSession: (spaceId: string | null) => call<void>("new_session", { spaceId }),
  renameSession: (id: string, title: string) =>
    call<string>("rename_session", { id, title }),
  deleteSession: (id: string) => call<boolean>("delete_session", { id }),
  pinSession: (id: string, pinned: boolean) =>
    call<boolean>("pin_session", { id, pinned }),
  archiveSession: (id: string, archived: boolean) =>
    call<boolean>("archive_session", { id, archived }),
  revealSession: (id: string) => call<string>("reveal_session", { id }),

  // 空间
  spaces: () => call<SpaceInfo[]>("spaces"),
  createSpace: (name: string, path: string, extraPaths: string[] = []) =>
    call<SpaceInfo>("create_space", { name, path, extraPaths }),
  /** `extraPaths: []` 表示清空附加目录；`null` 表示不动它。 */
  updateSpace: (
    id: string,
    name: string | null,
    path: string | null,
    extraPaths: string[] | null = null,
  ) => call<SpaceInfo>("update_space", { id, name, path, extraPaths }),
  deleteSpace: (id: string) => call<DeleteReport>("delete_space", { id }),

  // 搜索
  searchSessions: (query: string, includeArchived: boolean) =>
    call<SessionHits[]>("search_sessions", { query, includeArchived }),

  // provider
  providerSetup: () => call<ProviderSetup>("provider_setup"),
  saveProvider: (input: Record<string, unknown>) =>
    call<ProviderSetup>("save_provider", { input }),
  appInfo: () => call<AppInfo>("app_info"),
};

export type { SearchHit };
