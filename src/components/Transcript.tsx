import { useEffect, useRef } from "react";

import type { Entry } from "../lib/entries";

interface Props {
  entries: Entry[];
  busy: boolean;
  /** 搜索跳转后要高亮的那条；`token` 变化就重新闪一次。 */
  highlight: { index: number; token: number } | null;
  empty: React.ReactNode;
}

export default function Transcript({ entries, busy, highlight, empty }: Props) {
  const scroller = useRef<HTMLDivElement>(null);

  // 新条目来了就贴底：工具卡展开会让高度突变，不强制滚底就"流式回答跑了"。
  useEffect(() => {
    const node = scroller.current;
    if (!node) return;
    const nearBottom = node.scrollHeight - node.scrollTop - node.clientHeight < 160;
    // 用户往回翻历史时不要抢滚动条。
    if (nearBottom || entries.at(-1)?.kind === "user") {
      node.scrollTop = node.scrollHeight;
    }
  }, [entries]);

  useEffect(() => {
    if (!highlight) return;
    const node = scroller.current?.querySelector<HTMLElement>(
      `[data-entry="${highlight.index}"]`,
    );
    node?.scrollIntoView({ block: "center", behavior: "smooth" });
  }, [highlight]);

  return (
    <div className="transcript" ref={scroller}>
      {entries.length === 0 && <div className="empty">{empty}</div>}

      {entries.map((entry, index) => {
        const flash =
          highlight?.index === index ? " flash-hit" : "";

        if (entry.kind === "user") {
          return (
            <div className={`row user${flash}`} key={index} data-entry={index}>
              <div className="bubble">{entry.text}</div>
            </div>
          );
        }

        if (entry.kind === "assistant") {
          return (
            <div className={`row assistant${flash}`} key={index} data-entry={index}>
              <div className="bubble">{entry.text}</div>
            </div>
          );
        }

        if (entry.kind === "thinking") {
          // 还在流式的最后一块保持展开，结束那一帧自动收起；
          // 用户手动展开后不会再被事件推回去（对同一块不再有事件）。
          const streaming = index === entries.length - 1 && busy;
          return (
            <div className={`row thinking${flash}`} key={index} data-entry={index}>
              <details className="thinking-box" open={streaming}>
                <summary>
                  <span className="thinking-label">{streaming ? "思考中…" : "思考过程"}</span>
                  <span className="thinking-meta">{entry.text.length} 字</span>
                </summary>
                <pre className="thinking-text">{entry.text}</pre>
              </details>
            </div>
          );
        }

        if (entry.kind === "notice") {
          return (
            <div className={`notice ${entry.level}${flash}`} key={index} data-entry={index}>
              {entry.text}
            </div>
          );
        }

        return (
          <div className={`tool ${entry.status}${flash}`} key={entry.id} data-entry={index}>
            <div className="tool-head">
              <span className="tool-name">{entry.name}</span>
              <span className="tool-status">
                {entry.status === "running" ? "执行中" : entry.status === "ok" ? "完成" : "失败"}
              </span>
            </div>
            <pre className="tool-input">{JSON.stringify(entry.input, null, 2)}</pre>
            {entry.output !== undefined && <pre className="tool-output">{entry.output}</pre>}
          </div>
        );
      })}
    </div>
  );
}
