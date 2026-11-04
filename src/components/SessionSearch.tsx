import { useEffect, useRef, useState } from "react";

import Highlight from "./Highlight";

export interface EntryHit {
  index: number;
  snippet: string;
}

interface Props {
  query: string;
  onQuery: (value: string) => void;
  hits: EntryHit[];
  onPick: (index: number) => void;
  onClose: () => void;
}

/**
 * 当前会话搜索：搜的是**内存里的条目**，所以能搜到本次运行的思考块。
 *
 * 这正是它与全局搜索的真实差异，不是漏搜——面板上要标出来，
 * 否则用户会以为两个搜索有一个是坏的。
 */
export default function SessionSearch({ query, onQuery, hits, onPick, onClose }: Props) {
  const [selected, setSelected] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  useEffect(() => {
    setSelected(0);
  }, [query]);

  function onKeyDown(event: React.KeyboardEvent): void {
    if (event.key === "Escape") {
      event.preventDefault();
      onClose();
      return;
    }
    if (event.key === "ArrowDown" && hits.length > 0) {
      event.preventDefault();
      setSelected((value) => (value + 1) % hits.length);
      return;
    }
    if (event.key === "ArrowUp" && hits.length > 0) {
      event.preventDefault();
      setSelected((value) => (value - 1 + hits.length) % hits.length);
      return;
    }
    if (event.key === "Enter" && hits[selected]) {
      event.preventDefault();
      onPick(hits[selected].index);
    }
  }

  return (
    <div className="session-search" role="dialog" aria-label="搜索当前会话">
      <div className="search-head compact">
        <span className="search-ico" aria-hidden="true">
          ⌕
        </span>
        <input
          ref={inputRef}
          value={query}
          placeholder="在当前会话里找…"
          onChange={(event) => onQuery(event.target.value)}
          onKeyDown={onKeyDown}
        />
        <span className="search-scope">含本次思考</span>
        <kbd className="kbd">Esc</kbd>
      </div>

      <div className="search-body compact">
        {query.trim().length === 0 && (
          <p className="search-empty">只搜这一条会话。本次运行的思考过程也在这里。</p>
        )}
        {query.trim().length > 0 && hits.length === 0 && (
          <p className="search-empty">这一条会话里没有。</p>
        )}
        {hits.map((hit, index) => (
          <button
            type="button"
            key={hit.index}
            className={index === selected ? "hit selected" : "hit"}
            onMouseEnter={() => setSelected(index)}
            onClick={() => onPick(hit.index)}
          >
            <span className="hit-snippet">
              <Highlight text={hit.snippet} query={query} />
            </span>
          </button>
        ))}
      </div>
    </div>
  );
}
