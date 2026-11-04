import { useMemo } from "react";

import type { SessionSummary, SpaceInfo } from "../types";
import { relativeTime, shortPath } from "../lib/time";

interface Props {
  spaces: SpaceInfo[];
  /** 全部会话（含已归档）。分组与折叠都在这层做，不再发第二个请求。 */
  sessions: SessionSummary[];
  activeId: string | null;
  activeSpaceId: string | null;
  busy: boolean;
  archivedOpen: boolean;
  renaming: string | null;
  renameDraft: string;
  onRenameDraft: (value: string) => void;
  onCommitRename: () => void;
  onCancelRename: () => void;
  onOpenSession: (id: string) => void;
  onNewSession: (spaceId: string | null) => void;
  onSessionMenu: (session: SessionSummary, at: { x: number; y: number }) => void;
  onSpaceMenu: (space: SpaceInfo, at: { x: number; y: number }) => void;
  onCreateSpace: () => void;
  onToggleArchived: () => void;
  onResizeStart: (event: React.PointerEvent) => void;
  width: number;
}

function anchor(event: React.MouseEvent): { x: number; y: number } {
  const rect = (event.currentTarget as HTMLElement).getBoundingClientRect();
  return { x: rect.right, y: rect.bottom + 4 };
}

/** 置顶的排前面；其余按最近改过的排。排序只在渲染层做，不动存储。 */
function order(list: SessionSummary[]): SessionSummary[] {
  return [...list].sort((a, b) => {
    if (a.pinned !== b.pinned) return a.pinned ? -1 : 1;
    return b.updated_at_ms - a.updated_at_ms;
  });
}

interface RowProps {
  session: SessionSummary;
  active: boolean;
  busy: boolean;
  renaming: boolean;
  renameDraft: string;
  onRenameDraft: (value: string) => void;
  onCommitRename: () => void;
  onCancelRename: () => void;
  onOpen: () => void;
  onMenu: (at: { x: number; y: number }) => void;
}

function SessionRow({
  session,
  active,
  busy,
  renaming,
  renameDraft,
  onRenameDraft,
  onCommitRename,
  onCancelRename,
  onOpen,
  onMenu,
}: RowProps) {
  if (renaming) {
    return (
      <div className="session renaming">
        <input
          className="session-rename"
          value={renameDraft}
          autoFocus
          onChange={(event) => onRenameDraft(event.target.value)}
          onBlur={onCommitRename}
          onKeyDown={(event) => {
            if (event.key === "Enter") {
              event.preventDefault();
              onCommitRename();
            }
            if (event.key === "Escape") {
              event.preventDefault();
              onCancelRename();
            }
          }}
        />
      </div>
    );
  }

  return (
    <div
      className={active ? "session active" : "session"}
      onContextMenu={(event) => {
        event.preventDefault();
        onMenu({ x: event.clientX, y: event.clientY });
      }}
    >
      <button
        type="button"
        className="session-main"
        onClick={onOpen}
        disabled={busy || active}
        title={session.title}
      >
        <span className="session-title">
          {session.pinned && <span className="pin-dot" title="已置顶" />}
          {session.title}
        </span>
        <span className="session-meta">
          {session.archived ? "已归档 · " : ""}
          {session.messages} 条 · {relativeTime(session.updated_at_ms)}
        </span>
      </button>

      <button
        type="button"
        className="session-more"
        onClick={(event) => onMenu(anchor(event))}
        title="更多操作"
        aria-label="更多操作"
      >
        ⋯
      </button>
    </div>
  );
}

export default function Sidebar(props: Props) {
  const {
    spaces,
    sessions,
    activeId,
    activeSpaceId,
    busy,
    archivedOpen,
    renaming,
    renameDraft,
    onRenameDraft,
    onCommitRename,
    onCancelRename,
    onOpenSession,
    onNewSession,
    onSessionMenu,
    onSpaceMenu,
    onCreateSpace,
    onToggleArchived,
    onResizeStart,
    width,
  } = props;

  const { tasks, bySpace, archived } = useMemo(() => {
    const live = sessions.filter((session) => !session.archived);
    const groups = new Map<string, SessionSummary[]>();
    for (const space of spaces) groups.set(space.id, []);
    const tasks: SessionSummary[] = [];
    for (const session of live) {
      if (session.space_id && groups.has(session.space_id)) {
        groups.get(session.space_id)!.push(session);
      } else {
        // 空间已被删、但会话还没搬完：归到「任务」，比让它凭空消失好。
        tasks.push(session);
      }
    }
    return {
      tasks: order(tasks),
      bySpace: new Map([...groups].map(([id, list]) => [id, order(list)])),
      archived: order(sessions.filter((session) => session.archived)),
    };
  }, [sessions, spaces]);

  const renderRow = (session: SessionSummary) => (
    <SessionRow
      key={session.id}
      session={session}
      active={session.id === activeId}
      busy={busy}
      renaming={renaming === session.id}
      renameDraft={renameDraft}
      onRenameDraft={onRenameDraft}
      onCommitRename={onCommitRename}
      onCancelRename={onCancelRename}
      onOpen={() => onOpenSession(session.id)}
      onMenu={(at) => onSessionMenu(session, at)}
    />
  );

  return (
    <>
      <aside className="sidebar" style={{ width }}>
        <div className="sidebar-head">
          <span>会话</span>
          <div className="sidebar-head-actions">
            <span className="sidebar-count">{sessions.length}</span>
            <button
              type="button"
              className="icon-btn tiny"
              onClick={onCreateSpace}
              title="新建空间"
              aria-label="新建空间"
            >
              +
            </button>
          </div>
        </div>

        <div className="session-list">
          <section className="group">
            <header className="group-head">
              <span className="group-name">任务</span>
              <span className="group-count">{tasks.length}</span>
              <button
                type="button"
                className="icon-btn tiny"
                onClick={() => onNewSession(null)}
                disabled={busy}
                title="在「任务」里新建会话"
              >
                +
              </button>
            </header>
            {tasks.map(renderRow)}
            {tasks.length === 0 && <p className="group-empty">说第一句话就会出现在这里。</p>}
          </section>

          {spaces.map((space) => {
            const list = bySpace.get(space.id) ?? [];
            return (
              <section className="group" key={space.id}>
                <header
                  className={
                    activeSpaceId === space.id ? "group-head space active" : "group-head space"
                  }
                >
                  <div className="group-space-text" title={space.path}>
                    <span className="group-name">
                      {space.name}
                      {!space.available && <span className="tag-warn">目录不可用</span>}
                    </span>
                    <span className="group-path">{shortPath(space.path)}</span>
                  </div>
                  <span className="group-count">{list.length}</span>
                  <button
                    type="button"
                    className="icon-btn tiny"
                    onClick={() => onNewSession(space.id)}
                    disabled={busy || !space.available}
                    title="在这个空间里新建会话"
                  >
                    +
                  </button>
                  <button
                    type="button"
                    className="icon-btn tiny"
                    onClick={(event) => onSpaceMenu(space, anchor(event))}
                    title="空间操作"
                    aria-label="空间操作"
                  >
                    ⋯
                  </button>
                </header>
                <div className="space-children">
                  {list.map(renderRow)}
                  {list.length === 0 && <p className="group-empty">还没有会话。</p>}
                </div>
              </section>
            );
          })}
        </div>

        {archived.length > 0 && (
          <div className="archive">
            <button
              type="button"
              className="archive-toggle"
              onClick={onToggleArchived}
              aria-expanded={archivedOpen}
            >
              <span className={archivedOpen ? "caret open" : "caret"}>▸</span>
              已归档 ({archived.length})
            </button>
            {archivedOpen && <div className="archive-list">{archived.map(renderRow)}</div>}
          </div>
        )}
      </aside>

      {/* 9px 热区：负 margin 让它压在分隔线上而不占宽度 */}
      <div
        className="side-gutter"
        role="separator"
        aria-orientation="vertical"
        tabIndex={0}
        title="拖动调宽，双击复位"
        onPointerDown={onResizeStart}
      />
    </>
  );
}
