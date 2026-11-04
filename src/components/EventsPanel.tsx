import type { EventEnvelope } from "../types";

interface Props {
  events: EventEnvelope[];
  sessionId: string | null;
  onClose: () => void;
}

function shortId(id: string | null): string {
  if (!id) return "—";
  return id.length > 9 ? `…${id.slice(-6)}` : id;
}

/**
 * 事件流。
 *
 * 每条都标了 `#seq` 和会话短 id：事件串台时能一眼看出「这条属于谁」，
 * 丢帧时能看出「中间缺了几号」。这两件事以前只能靠猜。
 */
export default function EventsPanel({ events, sessionId, onClose }: Props) {
  return (
    <aside className="events">
      <div className="events-head">
        <span>
          事件流（{events.length}）
          {sessionId && <span className="events-sid">{shortId(sessionId)}</span>}
        </span>
        <button type="button" className="icon-btn tiny" onClick={onClose} aria-label="收起事件流">
          ✕
        </button>
      </div>
      <div className="events-body">
        {events.length === 0 && <p className="events-empty">还没有事件。发一句话试试。</p>}
        {events.map((envelope) => (
          <div className="ev" key={`${envelope.session_id}-${envelope.seq}`}>
            <div className="ev-top">
              <span className={`ev-type ev-${envelope.event.type}`}>{envelope.event.type}</span>
              <span className="ev-meta">
                #{envelope.seq} {shortId(envelope.session_id)}
              </span>
            </div>
            <div className="ev-payload">{JSON.stringify(envelope.event).slice(0, 180)}</div>
          </div>
        ))}
      </div>
    </aside>
  );
}
