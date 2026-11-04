import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";

import { api } from "./api";
import {
  applyEvent,
  entriesFromMessages,
  indexOfSource,
  searchEntries,
  type Entry,
} from "./lib/entries";
import type {
  AgentEvent,
  AppInfo,
  EventEnvelope,
  ProviderSetup,
  SearchHit,
  SessionHits,
  SessionSummary,
  Snapshot,
  SpaceInfo,
} from "./types";
import { MODE_LABELS, STOP_LABELS } from "./types";

import Topbar from "./components/Topbar";
import Sidebar from "./components/Sidebar";
import ChatHead from "./components/ChatHead";
import Transcript from "./components/Transcript";
import Composer from "./components/Composer";
import EventsPanel from "./components/EventsPanel";
import SettingsSheet from "./components/SettingsSheet";
import SessionMenu, { type MenuTarget } from "./components/SessionMenu";
import SpaceDialog from "./components/SpaceDialog";
import GlobalSearch from "./components/GlobalSearch";
import SessionSearch from "./components/SessionSearch";

const EVENT_CHANNEL = "agent://event";

const PREF = {
  sideWidth: "ga.sideWidth",
  sideCollapsed: "ga.sideCollapsed",
  eventsOpen: "ga.eventsOpen",
} as const;

function readPref(key: string): string | null {
  try {
    return window.localStorage.getItem(key);
  } catch {
    return null;
  }
}

function writePref(key: string, value: string): void {
  try {
    window.localStorage.setItem(key, value);
  } catch {
    // 隐私模式下 localStorage 可能直接抛错。偏好丢了不影响使用。
  }
}

/** 侧栏宽度：180–460，且不超过窗口的 52%。 */
function clampWidth(width: number): number {
  const max = Math.min(460, Math.floor(window.innerWidth * 0.52));
  if (!Number.isFinite(width)) return 244;
  return Math.max(180, Math.min(max, Math.round(width)));
}

export default function App() {
  const [snapshot, setSnapshot] = useState<Snapshot | null>(null);
  const [spaces, setSpaces] = useState<SpaceInfo[]>([]);
  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  const [activeId, setActiveId] = useState<string | null>(null);
  const [entries, setEntries] = useState<Entry[]>([]);
  const [events, setEvents] = useState<EventEnvelope[]>([]);
  const [input, setInput] = useState("");
  const [busy, setBusy] = useState(false);
  const [modes, setModes] = useState<string[]>([]);
  const [permission, setPermission] = useState<{
    id: string;
    name: string;
    summary: string;
  } | null>(null);

  const [sideCollapsed, setSideCollapsed] = useState(() => readPref(PREF.sideCollapsed) === "1");
  const [sideWidth, setSideWidth] = useState(() => clampWidth(Number(readPref(PREF.sideWidth) ?? 244)));
  const [eventsOpen, setEventsOpen] = useState(() => readPref(PREF.eventsOpen) === "1");
  const [archivedOpen, setArchivedOpen] = useState(false);

  const [menu, setMenu] = useState<{ target: MenuTarget; at: { x: number; y: number } } | null>(null);
  const [searchOpen, setSearchOpen] = useState(false);
  const [searchQuery, setSearchQuery] = useState("");
  const [searchResults, setSearchResults] = useState<SessionHits[]>([]);
  const [searchLoading, setSearchLoading] = useState(false);
  const [searchError, setSearchError] = useState<string | null>(null);

  const [sessionSearchOpen, setSessionSearchOpen] = useState(false);
  const [sessionQuery, setSessionQuery] = useState("");

  const [spaceDialog, setSpaceDialog] = useState<
    { mode: "create" } | { mode: "edit"; space: SpaceInfo } | null
  >(null);
  const [deleteTarget, setDeleteTarget] = useState<SessionSummary | null>(null);
  const [renaming, setRenaming] = useState<string | null>(null);
  const [renameDraft, setRenameDraft] = useState("");

  const [settingsOpen, setSettingsOpen] = useState(false);
  const [setup, setSetup] = useState<ProviderSetup | null>(null);
  const [info, setInfo] = useState<AppInfo | null>(null);

  const [flash, setFlash] = useState<{ index: number; token: number } | null>(null);
  const [toast, setToast] = useState<string | null>(null);

  const busyRef = useRef(false);
  const activeIdRef = useRef<string | null>(null);
  const renameGuard = useRef<string | null>(null);
  busyRef.current = busy;
  activeIdRef.current = activeId;

  const notify = useCallback((text: string) => {
    setToast(text);
    window.setTimeout(() => setToast((current) => (current === text ? null : current)), 2600);
  }, []);

  const refresh = useCallback(async () => {
    try {
      const next = await api.snapshot();
      setSnapshot(next);
      setActiveId(next.session_id);
      const [spaceList, sessionList] = await Promise.all([
        api.spaces(),
        // 一次取回全部（含归档）：侧栏是一棵树，取两次会出现半棵树。
        api.listSessions(null, true),
      ]);
      setSpaces(spaceList);
      setSessions(sessionList);
    } catch (error) {
      console.error("读取状态失败", error);
    }
  }, []);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let disposed = false;

    void listen<EventEnvelope>(EVENT_CHANNEL, (message) => {
      const envelope = message.payload;
      setEvents((prev) => [...prev.slice(-199), envelope]);

      // 只认当前会话的帧。信封就是为这件事存在的：
      // 以前靠「busy 时禁止切会话」保证不串台，那是 UI 约束，不是系统约束。
      if (envelope.session_id && activeIdRef.current && envelope.session_id !== activeIdRef.current) {
        return;
      }

      const event = envelope.event as AgentEvent;
      setEntries((prev) => applyEvent(prev, event));

      if (event.type === "permission_requested") {
        setPermission({ id: event.id, name: event.name, summary: event.summary });
      }
      if (event.type === "finished" || event.type === "error") {
        setPermission(null);
      }
    }).then((fn) => {
      if (disposed) fn();
      else unlisten = fn;
    });

    void api.modes().then((result) => {
      if (!disposed) setModes(result.all);
    });
    void api.providerSetup().then((result) => {
      if (!disposed) setSetup(result);
    });
    void api.appInfo().then((result) => {
      if (!disposed) setInfo(result);
    });

    // 首屏回填：后端启动时已经打开了最近一条会话，这里要把它的历史画出来，
    // 否则会出现「顶栏说 12 条消息、对话区却是空的」。
    void (async () => {
      await refresh();
      try {
        const history = await api.history();
        if (!disposed && history.length > 0) setEntries(entriesFromMessages(history));
      } catch (error) {
        console.error("读取历史失败", error);
      }
    })();

    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [refresh]);

  // 纯 UI 偏好留在这台机器这个窗口，不进后端配置——它们不是项目属性。
  useEffect(() => writePref(PREF.sideWidth, String(sideWidth)), [sideWidth]);
  useEffect(() => writePref(PREF.sideCollapsed, sideCollapsed ? "1" : "0"), [sideCollapsed]);
  useEffect(() => writePref(PREF.eventsOpen, eventsOpen ? "1" : "0"), [eventsOpen]);

  // 全局搜索：150ms debounce。每次敲键都扫盘会让首屏发抖。
  useEffect(() => {
    if (!searchOpen) return;
    const query = searchQuery.trim();
    if (!query) {
      setSearchResults([]);
      setSearchError(null);
      setSearchLoading(false);
      return;
    }
    setSearchLoading(true);
    const timer = window.setTimeout(() => {
      void api
        .searchSessions(query, false)
        .then((result) => {
          setSearchResults(result);
          setSearchError(null);
        })
        .catch((error: unknown) => {
          setSearchResults([]);
          setSearchError(error instanceof Error ? error.message : String(error));
        })
        .finally(() => setSearchLoading(false));
    }, 150);
    return () => window.clearTimeout(timer);
  }, [searchQuery, searchOpen]);

  // 快捷键：Ctrl/Cmd+B 收侧栏，Ctrl/Cmd+K 全局搜索。
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (!(event.ctrlKey || event.metaKey)) return;
      const key = event.key.toLowerCase();
      if (key === "b") {
        event.preventDefault();
        setSideCollapsed((value) => !value);
      }
      if (key === "k") {
        event.preventDefault();
        setSearchQuery("");
        setSearchOpen(true);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  useEffect(() => {
    if (!flash) return;
    const timer = window.setTimeout(() => setFlash(null), 2200);
    return () => window.clearTimeout(timer);
  }, [flash]);

  const sessionSearchHits = useMemo(
    () => (sessionQuery.trim() ? searchEntries(entries, sessionQuery) : []),
    [entries, sessionQuery],
  );

  const activeSession = sessions.find((session) => session.id === activeId) ?? null;
  const activeSpaceName =
    spaces.find((space) => space.id === snapshot?.active_space_id)?.name ?? "任务";
  const modeLabel = MODE_LABELS[snapshot?.mode ?? ""] ?? snapshot?.mode ?? "…";

  // ───────────────────────── 会话动作 ─────────────────────────

  async function openSession(id: string, jump?: SearchHit): Promise<void> {
    if (busyRef.current || id === activeId) return;
    try {
      const history = await api.openSession(id);
      const next = entriesFromMessages(history);
      setEntries(next);
      setEvents([]);
      setActiveId(id);
      setSessionSearchOpen(false);
      setSessionQuery("");
      if (jump) {
        const index = indexOfSource(next, { msg: jump.msg, block: jump.block });
        if (index >= 0) setFlash({ index, token: Date.now() });
      }
      await refresh();
    } catch (error) {
      notify(error instanceof Error ? error.message : String(error));
    }
  }

  async function newSession(spaceId: string | null): Promise<void> {
    if (busyRef.current) return;
    try {
      await api.newSession(spaceId);
      setEntries([]);
      setEvents([]);
      setActiveId(null);
      setSessionSearchOpen(false);
      setSessionQuery("");
      await refresh();
    } catch (error) {
      notify(error instanceof Error ? error.message : String(error));
    }
  }

  async function send(): Promise<void> {
    const prompt = input.trim();
    if (!prompt || busyRef.current) return;

    setInput("");
    setEntries((prev) => [...prev, { kind: "user", text: prompt }]);
    setBusy(true);

    try {
      const report = await api.chat(prompt);
      if (report.outcome !== "completed") {
        setEntries((prev) => [
          ...prev,
          {
            kind: "notice",
            level: "info",
            text: `本轮结束：${STOP_LABELS[report.outcome] ?? report.outcome}`,
          },
        ]);
      }
    } catch (error) {
      setEntries((prev) => [
        ...prev,
        { kind: "notice", level: "error", text: String(error) },
      ]);
    } finally {
      setBusy(false);
      setPermission(null);
      await refresh();
    }
  }

  async function answerPermission(allowed: boolean): Promise<void> {
    if (!permission) return;
    try {
      await api.resolvePermission(permission.id, allowed);
    } finally {
      setPermission(null);
    }
  }

  async function pin(session: SessionSummary, pinned: boolean): Promise<void> {
    // 乐观更新：只写一个零字节标记，失败的概率低到不值得等一个来回。
    setSessions((prev) =>
      prev.map((item) => (item.id === session.id ? { ...item, pinned } : item)),
    );
    try {
      await api.pinSession(session.id, pinned);
    } catch (error) {
      setSessions((prev) =>
        prev.map((item) => (item.id === session.id ? { ...item, pinned: !pinned } : item)),
      );
      notify(error instanceof Error ? error.message : String(error));
    }
  }

  async function archive(session: SessionSummary, archived: boolean): Promise<void> {
    setSessions((prev) =>
      prev.map((item) => (item.id === session.id ? { ...item, archived } : item)),
    );
    try {
      await api.archiveSession(session.id, archived);
      if (archived && session.id === activeId) {
        setEntries([]);
        setEvents([]);
        setActiveId(null);
      }
      await refresh();
    } catch (error) {
      setSessions((prev) =>
        prev.map((item) => (item.id === session.id ? { ...item, archived: !archived } : item)),
      );
      notify(error instanceof Error ? error.message : String(error));
    }
  }

  async function reveal(session: SessionSummary): Promise<void> {
    try {
      const path = await api.revealSession(session.id);
      notify(`已打开 ${path}`);
    } catch (error) {
      notify(error instanceof Error ? error.message : String(error));
    }
  }

  async function deleteForever(): Promise<void> {
    if (!deleteTarget) return;
    try {
      await api.deleteSession(deleteTarget.id);
      if (deleteTarget.id === activeId) {
        setEntries([]);
        setEvents([]);
        setActiveId(null);
      }
      await refresh();
    } catch (error) {
      notify(error instanceof Error ? error.message : String(error));
    } finally {
      setDeleteTarget(null);
    }
  }

  function startRename(session: SessionSummary): void {
    renameGuard.current = session.id;
    setRenaming(session.id);
    setRenameDraft(session.title);
  }

  async function commitRename(): Promise<void> {
    const id = renameGuard.current;
    if (!id) return;
    renameGuard.current = null;
    const title = renameDraft.trim();
    setRenaming(null);
    // 没改或者清空了就当取消——空标题后端也会拒。
    if (!title) return;
    try {
      await api.renameSession(id, title);
      await refresh();
    } catch (error) {
      notify(error instanceof Error ? error.message : String(error));
    }
  }

  function cancelRename(): void {
    renameGuard.current = null;
    setRenaming(null);
  }

  // ───────────────────────── 壳层交互 ─────────────────────────

  function startResize(event: React.PointerEvent): void {
    event.preventDefault();
    const startX = event.clientX;
    const startWidth = sideWidth;
    document.body.classList.add("resizing");

    const onMove = (moveEvent: PointerEvent) => {
      setSideWidth(clampWidth(startWidth + moveEvent.clientX - startX));
    };
    const onUp = () => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
      document.body.classList.remove("resizing");
    };
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
  }

  function pickSearchHit(sessionId: string, hit: SearchHit): void {
    setSearchOpen(false);
    void openSession(sessionId, hit);
  }

  const empty = (
    <>
      {snapshot?.provider_state === "unconfigured" ? (
        <>
          <p>还没接模型。</p>
          <p className="empty-hint">
            现在跑的是离线回显：你说什么它就把话回给你，工具与权限流程都能试。
            填一个 API Key 就能接真模型——配置只存在本机，不会上传到任何地方。
          </p>
          <button type="button" className="primary" onClick={() => setSettingsOpen(true)}>
            配置模型
          </button>
          {setup && <p className="empty-path">也可以自己编辑：{setup.config_path}</p>}
        </>
      ) : snapshot?.provider_state === "degraded" ? (
        <>
          <p>模型配置有问题，暂时退回离线回显。</p>
          <p className="empty-warn">{snapshot.provider_reason}</p>
          <button type="button" className="primary" onClick={() => setSettingsOpen(true)}>
            去修配置
          </button>
        </>
      ) : (
        <>
          <p>说点什么。</p>
          <p className="empty-hint">
            {snapshot?.provider === "mock"
              ? "当前显式配置为离线回显，它会把你的话原样回给你，工具照常可用。"
              : `已接入 ${snapshot?.model ?? ""}，可以读写「${activeSpaceName}」里的文件。`}
          </p>
        </>
      )}
    </>
  );

  return (
    <div
      className={sideCollapsed ? "app side-collapsed" : "app"}
      style={{ "--side-w": `${sideWidth}px` } as React.CSSProperties}
    >
      <Topbar
        snapshot={snapshot}
        modes={modes}
        busy={busy}
        sideCollapsed={sideCollapsed}
        eventsOpen={eventsOpen}
        onToggleSide={() => setSideCollapsed((value) => !value)}
        onToggleEvents={() => setEventsOpen((value) => !value)}
        onSearch={() => {
          setSearchQuery("");
          setSearchOpen(true);
        }}
        onNewSession={() => void newSession(snapshot?.active_space_id ?? null)}
        onOpenSettings={() => setSettingsOpen(true)}
        onChangeMode={(mode) => {
          void api.setMode(mode).finally(() => void refresh());
        }}
      />

      <main className="body">
        {!sideCollapsed && (
          <Sidebar
            spaces={spaces}
            sessions={sessions}
            activeId={activeId}
            activeSpaceId={snapshot?.active_space_id ?? null}
            busy={busy}
            archivedOpen={archivedOpen}
            renaming={renaming}
            renameDraft={renameDraft}
            onRenameDraft={setRenameDraft}
            onCommitRename={() => void commitRename()}
            onCancelRename={cancelRename}
            onOpenSession={(id) => void openSession(id)}
            onNewSession={(spaceId) => void newSession(spaceId)}
            onSessionMenu={(session, at) => setMenu({ target: { kind: "session", session }, at })}
            onSpaceMenu={(space, at) => setMenu({ target: { kind: "space", space }, at })}
            onCreateSpace={() => setSpaceDialog({ mode: "create" })}
            onToggleArchived={() => setArchivedOpen((value) => !value)}
            onResizeStart={startResize}
            width={sideWidth}
          />
        )}

        <section className="chat">
          <ChatHead
            title={activeSession?.title ?? "新对话"}
            workspace={snapshot?.workspace ?? ""}
            spaceName={activeSpaceName}
            searchOpen={sessionSearchOpen}
            onToggleSearch={() => {
              setSessionSearchOpen((value) => !value);
              setSessionQuery("");
            }}
          />

          {sessionSearchOpen && (
            <SessionSearch
              query={sessionQuery}
              onQuery={setSessionQuery}
              hits={sessionSearchHits}
              onPick={(index) => {
                setFlash({ index, token: Date.now() });
                setSessionSearchOpen(false);
              }}
              onClose={() => setSessionSearchOpen(false)}
            />
          )}

          <Transcript
            entries={entries}
            busy={busy}
            highlight={flash}
            empty={empty}
          />

          <Composer
            value={input}
            busy={busy}
            modeLabel={modeLabel}
            onChange={setInput}
            onSend={() => void send()}
            onCancel={() => void api.cancel()}
          />
        </section>

        {eventsOpen && (
          <EventsPanel
            events={events}
            sessionId={activeId}
            onClose={() => setEventsOpen(false)}
          />
        )}
      </main>

      {permission && (
        <div className="permission">
          <div>
            <strong>需要授权</strong>
            <span className="permission-summary">{permission.summary}</span>
          </div>
          <div className="permission-actions">
            <button type="button" className="primary" onClick={() => void answerPermission(true)}>
              允许
            </button>
            <button type="button" className="ghost" onClick={() => void answerPermission(false)}>
              拒绝
            </button>
          </div>
        </div>
      )}

      {menu && (
        <SessionMenu
          target={menu.target}
          at={menu.at}
          busy={busy}
          activeId={activeId}
          onClose={() => setMenu(null)}
          onPin={(session, pinned) => void pin(session, pinned)}
          onRename={startRename}
          onReveal={(session) => void reveal(session)}
          onArchive={(session, archived) => void archive(session, archived)}
          onDeleteForever={(session) => setDeleteTarget(session)}
          onEditSpace={(space) => setSpaceDialog({ mode: "edit", space })}
          onDeleteSpace={(space) => setSpaceDialog({ mode: "edit", space })}
        />
      )}

      {spaceDialog && (
        <SpaceDialog
          mode={spaceDialog.mode}
          space={spaceDialog.mode === "edit" ? spaceDialog.space : undefined}
          busy={busy}
          onClose={() => setSpaceDialog(null)}
          onSubmit={async (name, path) => {
            if (spaceDialog.mode === "create") {
              await api.createSpace(name, path);
            } else {
              await api.updateSpace(
                spaceDialog.space.id,
                name === spaceDialog.space.name ? null : name,
                path === spaceDialog.space.path ? null : path,
              );
            }
            setSpaceDialog(null);
            await refresh();
          }}
          onDelete={
            spaceDialog.mode === "edit"
              ? async (space) => {
                  await api.deleteSpace(space.id);
                  setSpaceDialog(null);
                  await refresh();
                }
              : undefined
          }
        />
      )}

      {searchOpen && (
        <GlobalSearch
          query={searchQuery}
          onQuery={setSearchQuery}
          results={searchResults}
          spaces={spaces}
          loading={searchLoading}
          error={searchError}
          onPick={pickSearchHit}
          onClose={() => setSearchOpen(false)}
        />
      )}

      {deleteTarget && (
        <div
          className="overlay"
          onMouseDown={(event) => {
            if (event.target === event.currentTarget) setDeleteTarget(null);
          }}
        >
          <div className="dlg small" role="dialog" aria-label="永久删除会话">
            <header className="dlg-head">
              <h2>永久删除</h2>
              <button
                type="button"
                className="icon-btn"
                onClick={() => setDeleteTarget(null)}
                aria-label="关闭"
              >
                ✕
              </button>
            </header>
            <div className="dlg-body">
              <p className="dlg-lead">
                「{deleteTarget.title}」会被从磁盘删掉，<strong>不能恢复</strong>。
                只是想让它离开列表的话，用「归档」。
              </p>
            </div>
            <footer className="dlg-foot">
              <button type="button" className="ghost" onClick={() => setDeleteTarget(null)}>
                取消
              </button>
              <button type="button" className="danger" onClick={() => void deleteForever()}>
                永久删除
              </button>
            </footer>
          </div>
        </div>
      )}

      {settingsOpen && (
        <SettingsSheet
          setup={setup}
          info={info}
          snapshot={snapshot}
          onClose={() => setSettingsOpen(false)}
          onSaved={(next) => {
            setSetup(next);
            setSettingsOpen(false);
            void refresh();
          }}
          onGoSpaces={() => {
            setSettingsOpen(false);
            setSpaceDialog({ mode: "create" });
          }}
          onNotice={notify}
        />
      )}

      {toast && <div className="toast">{toast}</div>}
    </div>
  );
}
