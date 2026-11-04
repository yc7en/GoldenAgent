import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

import {
  type AgentEvent,
  type AppInfo,
  type ChatReport,
  type Message,
  type ProviderSetup,
  type SessionSummary,
  type Snapshot,
  MODE_LABELS,
  PROVIDER_LABELS,
  STOP_LABELS,
} from "./types";

const EVENT_CHANNEL = "agent://event";

/** 工作区是怎么定下来的。用户觉得"看到的文件不对"时，先看这个。 */
const WORKSPACE_LABELS: Record<string, string> = {
  env: "环境变量指定",
  cwd: "当前目录",
  home: "家目录默认",
};

type Entry =
  | { kind: "user"; text: string }
  | { kind: "assistant"; text: string }
  /**
   * 推理模型的思考过程。
   *
   * 它是**临时展示**：不落盘、不进历史，所以只在本次运行的事件流里出现。
   * 单独一种条目而不是拼进 assistant，是因为它不该和回答长得一样——
   * 用户需要知道哪句是它想过的，哪句是它说的。
   */
  | { kind: "thinking"; text: string }
  | { kind: "notice"; level: "info" | "error"; text: string }
  | {
      kind: "tool";
      id: string;
      name: string;
      input: unknown;
      status: "running" | "ok" | "failed";
      output?: string;
    };

/**
 * 把一个事件折叠进对话列表。
 *
 * 前端**不判断业务**：它只是把事件画出来。
 * 这里唯一有点像"逻辑"的动作是"把连续的 text_delta 拼成一段"，
 * 那纯粹是渲染需要，不改变任何语义。
 */
function applyEvent(entries: Entry[], event: AgentEvent): Entry[] {
  switch (event.type) {
    case "text_delta": {
      const last = entries.at(-1);
      if (last?.kind === "assistant") {
        return [
          ...entries.slice(0, -1),
          { kind: "assistant", text: last.text + event.text },
        ];
      }
      return [...entries, { kind: "assistant", text: event.text }];
    }

    case "thinking_delta": {
      // 连续的思考片段并成一块，和 text_delta 一样只是为了渲染。
      const last = entries.at(-1);
      if (last?.kind === "thinking") {
        return [
          ...entries.slice(0, -1),
          { kind: "thinking", text: last.text + event.text },
        ];
      }
      return [...entries, { kind: "thinking", text: event.text }];
    }

    case "tool_started":
      return [
        ...entries,
        {
          kind: "tool",
          id: event.id,
          name: event.name,
          input: event.input,
          status: "running",
        },
      ];

    case "tool_finished":
      return entries.map((entry) =>
        entry.kind === "tool" && entry.id === event.id
          ? { ...entry, status: event.ok ? "ok" : "failed", output: event.output }
          : entry,
      );

    case "turn_started":
      // 第一轮不提示，否则每次对话都多一条噪音
      return event.turn > 1
        ? [
            ...entries,
            { kind: "notice", level: "info", text: `第 ${event.turn} 轮` },
          ]
        : entries;

    case "context_compacted":
      return [
        ...entries,
        {
          kind: "notice",
          level: "info",
          text: `上下文已压缩，移除 ${event.removed} 条历史`,
        },
      ];

    case "finished":
      return [
        ...entries,
        {
          kind: "notice",
          level: "info",
          text: `本轮结束：${STOP_LABELS[event.reason] ?? event.reason}`,
        },
      ];

    case "error":
      return [...entries, { kind: "notice", level: "error", text: event.message }];

    // permission_requested 单独用横幅展示，不进对话流
    default:
      return entries;
  }
}

/**
 * 把落盘的历史折成对话条目。
 *
 * 和 `applyEvent` 是同一件事的两个方向：那边是"事件流 → 条目"，
 * 这边是"已存的消息 → 条目"。工具卡片靠 `tool_call` / `tool_result` 的 id 配对，
 * 所以打开旧会话时能看到当时调用了什么、结果如何，而不是只剩文字。
 */
function entriesFromMessages(messages: Message[]): Entry[] {
  const entries: Entry[] = [];
  const slots = new Map<string, number>();

  for (const message of messages) {
    for (const block of message.blocks) {
      if (block.type === "text") {
        entries.push(
          message.role === "user"
            ? { kind: "user", text: block.text }
            : { kind: "assistant", text: block.text },
        );
      } else if (block.type === "tool_call") {
        slots.set(block.id, entries.length);
        entries.push({
          kind: "tool",
          id: block.id,
          name: block.name,
          input: block.input,
          status: "running",
        });
      } else {
        const slot = slots.get(block.id);
        if (slot === undefined) continue;
        const target = entries[slot];
        if (!target || target.kind !== "tool") continue;
        entries[slot] = {
          ...target,
          status: block.ok ? "ok" : "failed",
          output: block.output,
        };
      }
    }
  }

  return entries;
}

/** 侧栏里的时间只需要"多久以前"，精确到分钟就够了。 */
function relativeTime(ms: number): string {
  const diff = Date.now() - ms;
  if (diff < 0) return "刚刚";
  if (diff < 60_000) return "刚刚";
  if (diff < 3_600_000) return `${Math.floor(diff / 60_000)} 分钟前`;
  if (diff < 86_400_000) return `${Math.floor(diff / 3_600_000)} 小时前`;
  const date = new Date(ms);
  return `${date.getMonth() + 1}月${date.getDate()}日`;
}

export default function App() {
  const [entries, setEntries] = useState<Entry[]>([]);
  const [log, setLog] = useState<AgentEvent[]>([]);
  const [input, setInput] = useState("");
  const [busy, setBusy] = useState(false);
  const [permission, setPermission] = useState<{
    id: string;
    name: string;
    summary: string;
  } | null>(null);
  const [snapshot, setSnapshot] = useState<Snapshot | null>(null);
  const [modes, setModes] = useState<string[]>([]);
  const [showLog, setShowLog] = useState(false);

  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  const [activeId, setActiveId] = useState<string | null>(null);
  const [renaming, setRenaming] = useState<string | null>(null);
  const [renameDraft, setRenameDraft] = useState("");
  const [confirmDelete, setConfirmDelete] = useState<string | null>(null);

  const [setup, setSetup] = useState<ProviderSetup | null>(null);
  const [info, setInfo] = useState<AppInfo | null>(null);
  const [showSettings, setShowSettings] = useState(false);
  const [form, setForm] = useState({
    kind: "anthropic",
    model: "",
    api_key: "",
    base_url: "",
    // 用字符串存：输入框清空时不能立刻变成 0，那会被后端夹成最小值。
    max_tokens: "",
  });
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);

  const transcriptRef = useRef<HTMLDivElement>(null);
  const busyRef = useRef(false);
  const renameGuard = useRef<string | null>(null);
  busyRef.current = busy;

  const refresh = useCallback(async () => {
    try {
      const next = await invoke<Snapshot>("snapshot");
      setSnapshot(next);
      setActiveId(next.session_id);
      setSessions(await invoke<SessionSummary[]>("list_sessions"));
    } catch (error) {
      console.error("读取状态失败", error);
    }
  }, []);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let disposed = false;

    void listen<AgentEvent>(EVENT_CHANNEL, (message) => {
      const event = message.payload;
      setLog((prev) => [...prev.slice(-199), event]);
      setEntries((prev) => applyEvent(prev, event));

      if (event.type === "permission_requested") {
        setPermission({
          id: event.id,
          name: event.name,
          summary: event.summary,
        });
      }
      if (event.type === "finished" || event.type === "error") {
        setPermission(null);
      }
    }).then((fn) => {
      if (disposed) {
        fn();
      } else {
        unlisten = fn;
      }
    });

    void invoke<{ current: string; all: string[] }>("modes").then((result) =>
      setModes(result.all),
    );

    // 配置面板的数据只在挂载时取一次：它不会因为对话而变，
    // 变了只有一个原因——用户在面板里保存了，那时会主动重新取。
    void invoke<ProviderSetup>("provider_setup").then((result) => {
      if (!disposed) setSetup(result);
    });
    void invoke<AppInfo>("app_info").then((result) => {
      if (!disposed) setInfo(result);
    });

    // 首屏回填：后端启动时已经打开了最近一条会话，这里要把它的历史画出来，
    // 否则会出现"顶栏说 12 条消息、对话区却是空的"。
    void (async () => {
      await refresh();
      try {
        const history = await invoke<Message[]>("history");
        if (!disposed && history.length > 0) {
          setEntries(entriesFromMessages(history));
        }
      } catch (error) {
        console.error("读取历史失败", error);
      }
    })();

    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [refresh]);

  useEffect(() => {
    const node = transcriptRef.current;
    if (node) {
      node.scrollTop = node.scrollHeight;
    }
  }, [entries]);

  // 改名时提交一次就够：回车会先触发提交，紧接着的 blur 不该再写一遍。
  function startRename(session: SessionSummary) {
    renameGuard.current = session.id;
    setRenaming(session.id);
    setRenameDraft(session.title);
    setConfirmDelete(null);
  }

  async function commitRename() {
    const id = renameGuard.current;
    if (!id) return;
    renameGuard.current = null;

    const title = renameDraft.trim();
    setRenaming(null);

    // 没改或者清空了就当取消——空标题后端也会拒，没必要让它跑一趟再报错。
    if (title.length === 0) return;

    try {
      await invoke<string>("rename_session", { id, title });
    } catch (error) {
      setEntries((prev) => [
        ...prev,
        { kind: "notice", level: "error", text: String(error) },
      ]);
    } finally {
      void refresh();
    }
  }

  function cancelRename() {
    renameGuard.current = null;
    setRenaming(null);
  }

  async function openSession(id: string) {
    if (busyRef.current || id === activeId) return;
    try {
      const history = await invoke<Message[]>("open_session", { id });
      setEntries(entriesFromMessages(history));
      setLog([]);
      setActiveId(id);
      void refresh();
    } catch (error) {
      setEntries((prev) => [
        ...prev,
        { kind: "notice", level: "error", text: String(error) },
      ]);
    }
  }

  async function newSession() {
    if (busyRef.current) return;
    await invoke("new_session");
    setEntries([]);
    setLog([]);
    setActiveId(null);
    setConfirmDelete(null);
    void refresh();
  }

  async function removeSession(id: string) {
    try {
      await invoke<boolean>("delete_session", { id });
    } catch (error) {
      setEntries((prev) => [
        ...prev,
        { kind: "notice", level: "error", text: String(error) },
      ]);
    } finally {
      setConfirmDelete(null);
      void refresh();
    }
  }

  async function send() {
    const prompt = input.trim();
    if (!prompt || busyRef.current) {
      return;
    }

    setInput("");
    setEntries((prev) => [...prev, { kind: "user", text: prompt }]);
    setBusy(true);

    try {
      const report = await invoke<ChatReport>("chat", { prompt });
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
      void refresh();
    }
  }

  async function answerPermission(allowed: boolean) {
    if (!permission) {
      return;
    }
    try {
      await invoke<boolean>("resolve_permission", {
        id: permission.id,
        allowed,
      });
    } finally {
      setPermission(null);
    }
  }

  async function changeMode(mode: string) {
    try {
      await invoke<string>("set_mode", { mode });
    } finally {
      void refresh();
    }
  }

  /** 打开面板时用当前配置预填，但不填 key：后端根本不会把它发下来。 */
  function openSettings() {
    if (setup) {
      setForm({
        kind: setup.kind,
        model: setup.model,
        api_key: "",
        base_url: setup.base_url,
        max_tokens: String(setup.max_tokens),
      });
    }
    setSaveError(null);
    setShowSettings(true);
  }

  async function saveProvider() {
    if (saving) return;
    setSaving(true);
    setSaveError(null);

    try {
      const next = await invoke<ProviderSetup>("save_provider", {
        input: {
          kind: form.kind,
          model: form.model.trim(),
          // 空串表达"没改 key"。后端据此保留原来那份——
          // 界面从来拿不到旧 key，所以不能用"回填再提交"的办法。
          api_key: form.api_key.length > 0 ? form.api_key : null,
          base_url: form.base_url.trim(),
          // 空框 = 不改。填了非数字也当"不改"，别把一个 NaN 发给后端。
          max_tokens:
            form.max_tokens.trim().length > 0 &&
            Number.isFinite(Number(form.max_tokens))
              ? Number(form.max_tokens)
              : null,
        },
      });
      setSetup(next);
      setShowSettings(false);
      void refresh();
    } catch (error) {
      setSaveError(String(error));
    } finally {
      setSaving(false);
    }
  }

  async function copyTemplate() {
    if (!setup) return;
    try {
      await navigator.clipboard.writeText(setup.template);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1500);
    } catch {
      // 剪贴板被拒时不要静默：用户会以为复制成功了。
      setSaveError("复制失败，请展开下面的模板手动选择");
    }
  }

  const modeLabel = MODE_LABELS[snapshot?.mode ?? ""] ?? snapshot?.mode ?? "…";

  // 顶栏这句话要能一眼区分三件事：没配、配错、配好了。
  // 全说成"离线回显"的话，用户不知道该去配还是该去修。
  const providerState = snapshot?.provider_state ?? "unconfigured";
  const providerLabel = !snapshot
    ? "…"
    : providerState === "unconfigured"
      ? "未配置模型"
      : providerState === "degraded"
        ? "配置有问题"
        : snapshot.provider === "mock"
          ? "离线回显"
          : `${PROVIDER_LABELS[snapshot.provider] ?? snapshot.provider} · ${snapshot.model}`;

  // 表单里这几处提示都从后端下发的 catalog 推出来，不另抄一份默认模型名。
  const currentOption = setup?.catalog.find((option) => option.id === form.kind);
  const keyPlaceholder =
    setup?.kind === form.kind && providerState === "ready"
      ? "留空表示不修改已保存的 Key"
      : currentOption?.needs_key === false
        ? "这一项不需要 Key"
        : "必填";

  return (
    <div className="app">
      <header className="topbar">
        <div className="brand">
          <span className="brand-mark">G</span>
          <div>
            <h1>GoldenAgent</h1>
            <p className="brand-sub">
              {providerLabel} · {snapshot?.messages ?? 0} 条消息
            </p>
          </div>
        </div>

        <div className="controls">
          <label className="mode">
            <span>权限</span>
            <select
              value={snapshot?.mode ?? "ask"}
              onChange={(event) => void changeMode(event.target.value)}
              disabled={busy}
            >
              {(modes.length > 0 ? modes : ["ask"]).map((mode) => (
                <option key={mode} value={mode}>
                  {MODE_LABELS[mode] ?? mode}
                </option>
              ))}
            </select>
          </label>
          <button type="button" className="ghost" onClick={() => void newSession()}>
            新对话
          </button>
          <button
            type="button"
            className={showLog ? "ghost active" : "ghost"}
            onClick={() => setShowLog((value) => !value)}
          >
            事件
          </button>
          <button
            type="button"
            className={
              providerState === "ready" ? "ghost" : "ghost attention"
            }
            onClick={openSettings}
          >
            设置
          </button>
        </div>
      </header>

      <main className="body">
        <aside className="sidebar">
          <div className="sidebar-head">
            <span>会话</span>
            <span className="sidebar-count">{sessions.length}</span>
          </div>

          <div className="session-list">
            {sessions.length === 0 && (
              <p className="session-empty">还没有历史。说第一句话就会出现在这里。</p>
            )}

            {sessions.map((session) => {
              const active = session.id === activeId;
              const isRenaming = renaming === session.id;

              return (
                <div
                  className={active ? "session active" : "session"}
                  key={session.id}
                >
                  {isRenaming ? (
                    <input
                      className="session-rename"
                      value={renameDraft}
                      autoFocus
                      onChange={(event) => setRenameDraft(event.target.value)}
                      onBlur={() => void commitRename()}
                      onKeyDown={(event) => {
                        if (event.key === "Enter") {
                          event.preventDefault();
                          void commitRename();
                        }
                        if (event.key === "Escape") {
                          event.preventDefault();
                          cancelRename();
                        }
                      }}
                    />
                  ) : (
                    <>
                      <button
                        type="button"
                        className="session-main"
                        onClick={() => void openSession(session.id)}
                        disabled={busy || active}
                        title={session.title}
                      >
                        <span className="session-title">{session.title}</span>
                        <span className="session-meta">
                          {session.messages} 条 · {relativeTime(session.updated_at_ms)}
                        </span>
                      </button>

                      <div className="session-actions">
                        <button
                          type="button"
                          className="icon"
                          onClick={() => startRename(session)}
                          title="改名"
                        >
                          改名
                        </button>
                        {confirmDelete === session.id ? (
                          <button
                            type="button"
                            className="icon danger"
                            onClick={() => void removeSession(session.id)}
                            title="再点一次确认删除"
                          >
                            确认
                          </button>
                        ) : (
                          <button
                            type="button"
                            className="icon"
                            onClick={() => setConfirmDelete(session.id)}
                            title="删除这条会话"
                          >
                            删除
                          </button>
                        )}
                      </div>
                    </>
                  )}
                </div>
              );
            })}
          </div>
        </aside>

        <div className="transcript" ref={transcriptRef}>
          {entries.length === 0 && (
            <div className="empty">
              {providerState === "unconfigured" ? (
                <>
                  <p>还没接模型。</p>
                  <p className="empty-hint">
                    现在跑的是离线回显：你说什么它就把话回给你，工具与权限流程都能试。
                    填一个 API Key 就能接真模型——配置只存在本机，不会上传到任何地方。
                  </p>
                  <button
                    type="button"
                    className="primary"
                    onClick={openSettings}
                  >
                    配置模型
                  </button>
                  {setup && (
                    <p className="empty-path">
                      也可以自己编辑：{setup.config_path}
                    </p>
                  )}
                </>
              ) : providerState === "degraded" ? (
                <>
                  <p>模型配置有问题，暂时退回离线回显。</p>
                  <p className="empty-warn">{snapshot?.provider_reason}</p>
                  <button
                    type="button"
                    className="primary"
                    onClick={openSettings}
                  >
                    去修配置
                  </button>
                </>
              ) : (
                <>
                  <p>说点什么。</p>
                  <p className="empty-hint">
                    {snapshot?.provider === "mock"
                      ? "当前显式配置为离线回显，它会把你的话原样回给你，工具照常可用。"
                      : `已接入 ${snapshot?.model ?? ""}，可以读写工作区内的文件。`}
                  </p>
                </>
              )}
            </div>
          )}

          {entries.map((entry, index) => {
            if (entry.kind === "user") {
              return (
                <div className="row user" key={index}>
                  <div className="bubble">{entry.text}</div>
                </div>
              );
            }

            if (entry.kind === "assistant") {
              return (
                <div className="row assistant" key={index}>
                  <div className="bubble">{entry.text}</div>
                </div>
              );
            }

            if (entry.kind === "thinking") {
              // 还在流式的最后一块保持展开，结束那一帧自动收起；
              // 用户手动展开后不会再被事件推回去（对同一块不再有事件）。
              const streaming = index === entries.length - 1 && busy;
              return (
                <div className="row thinking" key={index}>
                  <details className="thinking-box" open={streaming}>
                    <summary>
                      <span className="thinking-label">
                        {streaming ? "思考中…" : "思考过程"}
                      </span>
                      <span className="thinking-meta">{entry.text.length} 字</span>
                    </summary>
                    <pre className="thinking-text">{entry.text}</pre>
                  </details>
                </div>
              );
            }

            if (entry.kind === "notice") {
              return (
                <div
                  className={`notice ${entry.level}`}
                  key={index}
                >
                  {entry.text}
                </div>
              );
            }

            return (
              <div className={`tool ${entry.status}`} key={entry.id}>
                <div className="tool-head">
                  <span className="tool-name">{entry.name}</span>
                  <span className="tool-status">
                    {entry.status === "running"
                      ? "执行中"
                      : entry.status === "ok"
                        ? "完成"
                        : "失败"}
                  </span>
                </div>
                <pre className="tool-input">
                  {JSON.stringify(entry.input, null, 2)}
                </pre>
                {entry.output !== undefined && (
                  <pre className="tool-output">{entry.output}</pre>
                )}
              </div>
            );
          })}
        </div>

        {showLog && (
          <aside className="log">
            <div className="log-head">事件流（{log.length}）</div>
            <div className="log-body">
              {log.map((event, index) => (
                <div className="log-line" key={index}>
                  <span className="log-type">{event.type}</span>
                  <span className="log-payload">
                    {JSON.stringify(event).slice(0, 160)}
                  </span>
                </div>
              ))}
            </div>
          </aside>
        )}
      </main>

      {permission && (
        <div className="permission">
          <div>
            <strong>需要授权</strong>
            <span className="permission-summary">{permission.summary}</span>
          </div>
          <div className="permission-actions">
            <button
              type="button"
              className="primary"
              onClick={() => void answerPermission(true)}
            >
              允许
            </button>
            <button
              type="button"
              className="ghost"
              onClick={() => void answerPermission(false)}
            >
              拒绝
            </button>
          </div>
        </div>
      )}

      {showSettings && (
        <div
          className="sheet-backdrop"
          onMouseDown={(event) => {
            // 只在点空白处关闭；拖选文字时松手落在遮罩上不该误关。
            if (event.target === event.currentTarget) setShowSettings(false);
          }}
        >
          <div className="sheet" role="dialog" aria-label="设置">
            <header className="sheet-head">
              <h2>设置</h2>
              <button
                type="button"
                className="icon"
                onClick={() => setShowSettings(false)}
              >
                关闭
              </button>
            </header>

            <div className="sheet-body">
              <section>
                <h3>模型</h3>
                <p className="sheet-note">
                  保存后立即生效，不用重启。配置写在下面的文件里，只存在本机。
                </p>

                <label className="field">
                  <span>厂商</span>
                  <select
                    value={form.kind}
                    onChange={(event) => {
                      const kind = event.target.value;
                      const option = setup?.catalog.find((o) => o.id === kind);
                      setForm((prev) => ({
                        ...prev,
                        kind,
                        // 换厂商就把模型名重置成该厂商的默认值；
                        // 留着上一家的模型名只会换来一个 404。
                        model: option?.default_model ?? "",
                      }));
                    }}
                  >
                    {(setup?.catalog ?? [{ id: "mock", default_model: "mock", needs_key: false }]).map(
                      (option) => (
                        <option key={option.id} value={option.id}>
                          {PROVIDER_LABELS[option.id] ?? option.id}
                        </option>
                      ),
                    )}
                  </select>
                </label>

                <label className="field">
                  <span>模型名</span>
                  <input
                    value={form.model}
                    placeholder={currentOption?.default_model ?? "留空使用厂商默认"}
                    onChange={(event) =>
                      setForm((prev) => ({ ...prev, model: event.target.value }))
                    }
                  />
                </label>

                <label className="field">
                  <span>API Key</span>
                  <input
                    type="password"
                    value={form.api_key}
                    placeholder={keyPlaceholder}
                    autoComplete="off"
                    disabled={currentOption?.needs_key === false}
                    onChange={(event) =>
                      setForm((prev) => ({ ...prev, api_key: event.target.value }))
                    }
                  />
                </label>

                <label className="field">
                  <span>接口地址</span>
                  <input
                    value={form.base_url}
                    placeholder="留空使用官方地址；自建网关/代理时填这里"
                    onChange={(event) =>
                      setForm((prev) => ({ ...prev, base_url: event.target.value }))
                    }
                  />
                </label>

                <label className="field">
                  <span>最大输出 tokens</span>
                  <input
                    type="number"
                    min={256}
                    max={32768}
                    step={256}
                    value={form.max_tokens}
                    placeholder={String(setup?.max_tokens ?? 8192)}
                    onChange={(event) =>
                      setForm((prev) => ({ ...prev, max_tokens: event.target.value }))
                    }
                  />
                </label>
                <p className="sheet-note">
                  推理模型的思考过程也占用这份额度。额度被思考吃光时，正文一个字
                  都不会出现——碰到"全用在思考上"的报错，先把它调大。
                </p>

                {saveError && <p className="sheet-error">{saveError}</p>}

                <div className="sheet-actions">
                  <button
                    type="button"
                    className="primary"
                    onClick={() => void saveProvider()}
                    disabled={saving}
                  >
                    {saving ? "保存中…" : "保存并生效"}
                  </button>
                </div>
              </section>

              <section>
                <h3>配置文件</h3>
                <code className="sheet-path">{setup?.config_path ?? "—"}</code>
                <p className="sheet-note">
                  手动改这个文件也可以，改完重启生效。里面的 Key 是明文，
                  别把它提交到仓库。
                </p>
                <div className="sheet-actions">
                  <button
                    type="button"
                    className="ghost"
                    onClick={() => void copyTemplate()}
                  >
                    复制模板
                  </button>
                  {copied && <span className="sheet-ok">已复制</span>}
                </div>
                <details>
                  <summary>看模板</summary>
                  <pre className="sheet-pre">{setup?.template ?? ""}</pre>
                </details>
              </section>

              <section>
                <h3>关于</h3>
                <dl className="sheet-facts">
                  <dt>版本</dt>
                  <dd>{info?.version ?? snapshot?.version ?? "—"}</dd>
                  <dt>标识</dt>
                  <dd>{info?.identifier ?? "—"}</dd>
                  <dt>工作区</dt>
                  <dd>
                    {info?.workspace ?? "—"}
                    {info && (
                      <span className="sheet-tag">
                        {WORKSPACE_LABELS[info.workspace_origin] ??
                          info.workspace_origin}
                      </span>
                    )}
                  </dd>
                </dl>
                {info?.update_url && (
                  <a
                    className="sheet-link"
                    href={info.update_url}
                    target="_blank"
                    rel="noreferrer"
                  >
                    检查更新
                  </a>
                )}
              </section>
            </div>
          </div>
        </div>
      )}

      <footer className="composer">
        <textarea
          value={input}
          placeholder={`给 GoldenAgent 说点什么（当前权限：${modeLabel}）`}
          rows={3}
          onChange={(event) => setInput(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Enter" && !event.shiftKey) {
              event.preventDefault();
              void send();
            }
          }}
        />
        <div className="composer-actions">
          <span className="hint">
            {busy ? "运行中…" : "Enter 发送 · Shift+Enter 换行"}
          </span>
          {busy ? (
            <button
              type="button"
              className="ghost"
              onClick={() => void invoke("cancel")}
            >
              停止
            </button>
          ) : (
            <button
              type="button"
              className="primary"
              onClick={() => void send()}
              disabled={input.trim().length === 0}
            >
              发送
            </button>
          )}
        </div>
      </footer>
    </div>
  );
}
