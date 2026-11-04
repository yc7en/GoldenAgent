import type { Snapshot } from "../types";
import { MODE_LABELS, PROVIDER_LABELS } from "../types";

interface Props {
  snapshot: Snapshot | null;
  modes: string[];
  busy: boolean;
  sideCollapsed: boolean;
  eventsOpen: boolean;
  onToggleSide: () => void;
  onToggleEvents: () => void;
  onSearch: () => void;
  onNewSession: () => void;
  onOpenSettings: () => void;
  onChangeMode: (mode: string) => void;
}

/** 顶栏这句话要能一眼区分三件事：没配、配错、配好了。 */
function providerLabel(snapshot: Snapshot | null): string {
  if (!snapshot) return "…";
  if (snapshot.provider_state === "unconfigured") return "未配置模型";
  if (snapshot.provider_state === "degraded") return "配置有问题";
  if (snapshot.provider === "mock") return "离线回显";
  return `${PROVIDER_LABELS[snapshot.provider] ?? snapshot.provider} · ${snapshot.model}`;
}

export default function Topbar({
  snapshot,
  modes,
  busy,
  sideCollapsed,
  eventsOpen,
  onToggleSide,
  onToggleEvents,
  onSearch,
  onNewSession,
  onOpenSettings,
  onChangeMode,
}: Props) {
  const state = snapshot?.provider_state ?? "unconfigured";

  return (
    <header className="topbar">
      <div className="topbar-left">
        <button
          type="button"
          className={sideCollapsed ? "icon-btn nav-toggle off" : "icon-btn nav-toggle"}
          onClick={onToggleSide}
          title={`${sideCollapsed ? "展开" : "收起"}侧边栏（Ctrl+B）`}
          aria-label="开关侧边栏"
        >
          <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true">
            <rect x="2" y="3" width="12" height="10" rx="2" fill="none" stroke="currentColor" />
            <line x1="6" y1="3" x2="6" y2="13" stroke="currentColor" />
            {sideCollapsed ? (
              <polyline points="9,6.5 11,8 9,9.5" fill="none" stroke="currentColor" />
            ) : (
              <polyline points="11,6.5 9,8 11,9.5" fill="none" stroke="currentColor" />
            )}
          </svg>
        </button>

        <div className="brand">
          <span className="brand-mark">G</span>
          <div className="brand-text">
            <h1>GoldenAgent</h1>
            <p className="brand-sub">
              {providerLabel(snapshot)} · {snapshot?.messages ?? 0} 条
            </p>
          </div>
        </div>
      </div>

      <div className="controls">
        <button
          type="button"
          className="icon-btn"
          onClick={onSearch}
          title="搜索全部会话（Ctrl+K）"
          aria-label="搜索全部会话"
        >
          <svg width="15" height="15" viewBox="0 0 16 16" aria-hidden="true">
            <circle cx="7" cy="7" r="4.5" fill="none" stroke="currentColor" />
            <line x1="10.5" y1="10.5" x2="14" y2="14" stroke="currentColor" />
          </svg>
        </button>

        <button
          type="button"
          className={eventsOpen ? "icon-btn on" : "icon-btn"}
          onClick={onToggleEvents}
          title={`${eventsOpen ? "收起" : "展开"}事件流`}
          aria-label="开关事件流"
        >
          <svg width="15" height="15" viewBox="0 0 16 16" aria-hidden="true">
            <line x1="2" y1="4" x2="14" y2="4" stroke="currentColor" />
            <line x1="2" y1="8" x2="10" y2="8" stroke="currentColor" />
            <line x1="2" y1="12" x2="12" y2="12" stroke="currentColor" />
          </svg>
        </button>

        <span className="topbar-sep" />

        <label className="mode">
          <span>权限</span>
          <select
            value={snapshot?.mode ?? "ask"}
            onChange={(event) => onChangeMode(event.target.value)}
            disabled={busy}
          >
            {(modes.length > 0 ? modes : ["ask"]).map((mode) => (
              <option key={mode} value={mode}>
                {MODE_LABELS[mode] ?? mode}
              </option>
            ))}
          </select>
        </label>

        <button type="button" className="ghost" onClick={onNewSession} disabled={busy}>
          新对话
        </button>
        <button
          type="button"
          className={state === "ready" ? "ghost" : "ghost attention"}
          onClick={onOpenSettings}
        >
          设置
        </button>
      </div>
    </header>
  );
}
