import { useEffect, useMemo, useRef, useState } from "react";

import type { SearchHit, SessionHits, SpaceInfo } from "../types";
import { relativeTime } from "../lib/time";
import Highlight from "./Highlight";

interface Props {
  query: string;
  onQuery: (value: string) => void;
  results: SessionHits[];
  spaces: SpaceInfo[];
  loading: boolean;
  error: string | null;
  onPick: (sessionId: string, hit: SearchHit) => void;
  onClose: () => void;
}

interface Flat {
  sessionId: string;
  hit: SearchHit;
  title: string;
  spaceId: string | null;
  updatedAt: number;
}

/**
 * 全局搜索：搜**落盘内容**。
 *
 * 口径必须写在空态里——否则用户搜不到刚看到的一句话（还没落盘的思考）
 * 会以为是坏了：
 * - 搜用户消息、助手正文、工具入参、工具输出；
 * - 不搜思考过程（它本来就不落盘）；
 * - 不搜已归档会话。
 */
export default function GlobalSearch({
  query,
  onQuery,
  results,
  spaces,
  loading,
  error,
  onPick,
  onClose,
}: Props) {
  const [selected, setSelected] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  const flat = useMemo<Flat[]>(
    () =>
      results.flatMap((group) =>
        group.hits.map((hit) => ({
          sessionId: group.session_id,
          hit,
          title: group.title,
          spaceId: group.space_id,
          updatedAt: group.updated_at_ms,
        })),
      ),
    [results],
  );

  useEffect(() => {
    setSelected(0);
  }, [query]);

  const spaceName = (id: string | null): string => {
    if (!id) return "任务";
    return spaces.find((space) => space.id === id)?.name ?? "已删除的空间";
  };

  function onKeyDown(event: React.KeyboardEvent): void {
    if (event.key === "Escape") {
      event.preventDefault();
      onClose();
      return;
    }
    if (event.key === "ArrowDown" && flat.length > 0) {
      event.preventDefault();
      setSelected((value) => (value + 1) % flat.length);
      return;
    }
    if (event.key === "ArrowUp" && flat.length > 0) {
      event.preventDefault();
      setSelected((value) => (value - 1 + flat.length) % flat.length);
      return;
    }
    if (event.key === "Enter" && flat[selected]) {
      event.preventDefault();
      onPick(flat[selected].sessionId, flat[selected].hit);
    }
  }

  return (
    <div
      className="overlay search-overlay"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget) onClose();
      }}
    >
      <div className="search-panel" role="dialog" aria-label="搜索全部会话">
        <div className="search-head">
          <span className="search-ico" aria-hidden="true">
            ⌕
          </span>
          <input
            ref={inputRef}
            value={query}
            placeholder="搜索全部会话：消息、工具入参、输出…"
            onChange={(event) => onQuery(event.target.value)}
            onKeyDown={onKeyDown}
          />
          <kbd className="kbd">Esc</kbd>
        </div>

        <div className="search-body">
          {error && <p className="search-empty err">{error}</p>}

          {!error && query.trim().length === 0 && (
            <p className="search-empty">
              搜的是<strong>落盘的历史</strong>：你发的、它说的、工具读写过的内容。
              思考过程不落盘，所以这里搜不到——本次运行的思考请在对话头那个搜索里找。
              已归档的会话不参与。
            </p>
          )}

          {!error && query.trim().length > 0 && loading && (
            <p className="search-empty">搜索中…</p>
          )}

          {!error && query.trim().length > 0 && !loading && flat.length === 0 && (
            <p className="search-empty">没有命中。换个词试试，或者它还没落盘。</p>
          )}

          {flat.map((item, index) => (
            <button
              type="button"
              key={`${item.sessionId}-${item.hit.msg}-${item.hit.block}`}
              className={index === selected ? "hit selected" : "hit"}
              onMouseEnter={() => setSelected(index)}
              onClick={() => onPick(item.sessionId, item.hit)}
            >
              <span className="hit-top">
                <span className={`role role-${item.hit.role}`}>
                  {item.hit.role === "user" ? "我" : "它"}
                </span>
                <span className="hit-title">{item.title}</span>
                <span className="hit-space">{spaceName(item.spaceId)}</span>
                <span className="hit-time">{relativeTime(item.updatedAt)}</span>
              </span>
              <span className="hit-snippet">
                <Highlight text={item.hit.snippet} query={query} />
              </span>
            </button>
          ))}
        </div>

        <div className="search-foot">
          <span>↑↓ 选择 · Enter 跳转 · Esc 关闭</span>
          <span>{flat.length > 0 ? `${flat.length} 处命中` : ""}</span>
        </div>
      </div>
    </div>
  );
}
