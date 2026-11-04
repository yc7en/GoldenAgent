import { useEffect, useRef } from "react";

import type { SessionSummary, SpaceInfo } from "../types";
import { shortPath } from "../lib/time";

export type MenuTarget =
  | { kind: "session"; session: SessionSummary }
  | { kind: "space"; space: SpaceInfo };

interface Props {
  target: MenuTarget;
  at: { x: number; y: number };
  busy: boolean;
  /** 当前会话 id：归档当前会话等于切会话，运行中要禁掉。 */
  activeId: string | null;
  onClose: () => void;
  onPin: (session: SessionSummary, pinned: boolean) => void;
  onRename: (session: SessionSummary) => void;
  onReveal: (session: SessionSummary) => void;
  onArchive: (session: SessionSummary, archived: boolean) => void;
  onDeleteForever: (session: SessionSummary) => void;
  onEditSpace: (space: SpaceInfo) => void;
  onDeleteSpace: (space: SpaceInfo) => void;
}

/**
 * 会话/空间的上下文菜单。
 *
 * 会话菜单里**没有删除**：归档是可恢复的软删除，永久删除放在归档区
 * （用户已经明确「这条不要了」的地方）。两件事混在一个菜单里，
 * 迟早会有人点错。
 */
export default function SessionMenu({
  target,
  at,
  busy,
  activeId,
  onClose,
  onPin,
  onRename,
  onReveal,
  onArchive,
  onDeleteForever,
  onEditSpace,
  onDeleteSpace,
}: Props) {
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const onPointerDown = (event: PointerEvent) => {
      if (!ref.current?.contains(event.target as Node)) onClose();
    };
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("pointerdown", onPointerDown, true);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("pointerdown", onPointerDown, true);
      window.removeEventListener("keydown", onKey);
    };
  }, [onClose]);

  // 贴着右下角时菜单会超出窗口：往回收，保证整块可见。
  const width = 208;
  const height = target.kind === "space" ? 116 : 172;
  const left = Math.min(at.x, window.innerWidth - width - 8);
  const top = Math.min(at.y, window.innerHeight - height - 8);

  const item = (
    label: string,
    onClick: () => void,
    options: { hint?: string; danger?: boolean; disabled?: boolean } = {},
  ) => (
    <button
      key={label}
      type="button"
      className={options.danger ? "menu-item danger" : "menu-item"}
      onClick={() => {
        if (options.disabled) return;
        onClick();
        onClose();
      }}
      disabled={options.disabled}
    >
      <span className="mi-name">{label}</span>
      {options.hint && <span className="mi-desc">{options.hint}</span>}
    </button>
  );

  return (
    <div className="menu" style={{ left, top, width }} ref={ref} role="menu">
      {target.kind === "session" ? (
        target.session.archived ? (
          <>
            {item("恢复", () => onArchive(target.session, false), {
              disabled: busy,
            })}
            {item("永久删除", () => onDeleteForever(target.session), {
              danger: true,
              hint: "不可恢复",
            })}
          </>
        ) : (
          <>
            {item(target.session.pinned ? "取消置顶" : "置顶", () =>
              onPin(target.session, !target.session.pinned),
            )}
            {item("重命名", () => onRename(target.session))}
            {item("打开文件夹", () => onReveal(target.session), {
              hint: shortPath(target.session.workspace),
            })}
            {item("归档", () => onArchive(target.session, true), {
              // 归档当前会话会触发切会话；运行中不能切。
              disabled: busy && target.session.id === activeId,
              hint: busy && target.session.id === activeId ? "运行中不能归档当前会话" : undefined,
            })}
          </>
        )
      ) : (
        <>
          {item("改名 / 换目录", () => onEditSpace(target.space))}
          {item("删除空间", () => onDeleteSpace(target.space), {
            danger: true,
            hint: `${target.space.sessions} 条会话会移回「任务」`,
          })}
        </>
      )}
    </div>
  );
}
