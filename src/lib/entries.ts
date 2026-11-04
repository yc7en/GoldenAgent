/**
 * 对话条目：事件流与落盘历史折叠成的同一套形状。
 *
 * 两种来源、一种渲染：
 * - `applyEvent`：正在发生什么（事件流）；
 * - `entriesFromMessages`：已经发生过什么（历史）。
 *
 * `src` 记的是这条条目对应落盘的哪个 `(msg, block)`。全局搜索命中后
 * 要能跳回来，就必须有这张映射——工具卡是 `tool_call` + `tool_result`
 * 两张卡配对渲染的，按「第几条」定位一定会错位。
 */

import type { AgentEvent, ContentBlock, Message } from "../types";
import { STOP_LABELS } from "../types";

export interface SourceRef {
  msg: number;
  block: number;
}

export type Entry =
  | { kind: "user"; text: string; src?: SourceRef }
  | { kind: "assistant"; text: string; src?: SourceRef }
  /**
   * 推理模型的思考过程。
   *
   * 它是**临时展示**：不落盘、不进历史，所以只在本次运行的事件流里出现。
   * 单独一种条目而不是拼进 assistant，是因为它不该和回答长得一样。
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
      src?: SourceRef;
    };

/**
 * 把一个事件折叠进对话列表。
 *
 * 前端**不判断业务**：它只是把事件画出来。这里唯一有点像"逻辑"的动作
 * 是"把连续的 text_delta 拼成一段"，那纯粹是渲染需要。
 */
export function applyEvent(entries: Entry[], event: AgentEvent): Entry[] {
  switch (event.type) {
    case "text_delta": {
      const last = entries.at(-1);
      if (last?.kind === "assistant") {
        return [...entries.slice(0, -1), { ...last, text: last.text + event.text }];
      }
      return [...entries, { kind: "assistant", text: event.text }];
    }

    case "thinking_delta": {
      const last = entries.at(-1);
      if (last?.kind === "thinking") {
        return [...entries.slice(0, -1), { ...last, text: last.text + event.text }];
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
        ? [...entries, { kind: "notice", level: "info", text: `第 ${event.turn} 轮` }]
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

/** 把落盘的历史折成对话条目，并记下每条对应的 (msg, block)。 */
export function entriesFromMessages(messages: Message[]): Entry[] {
  const entries: Entry[] = [];
  const slots = new Map<string, number>();

  messages.forEach((message, msg) => {
    message.blocks.forEach((block: ContentBlock, index) => {
      const src = { msg, block: index };

      if (block.type === "text") {
        entries.push(
          message.role === "user"
            ? { kind: "user", text: block.text, src }
            : { kind: "assistant", text: block.text, src },
        );
        return;
      }

      if (block.type === "tool_call") {
        slots.set(block.id, entries.length);
        entries.push({
          kind: "tool",
          id: block.id,
          name: block.name,
          input: block.input,
          status: "running",
          src,
        });
        return;
      }

      const slot = slots.get(block.id);
      if (slot === undefined) return;
      const target = entries[slot];
      if (!target || target.kind !== "tool") return;
      entries[slot] = {
        ...target,
        status: block.ok ? "ok" : "failed",
        output: block.output,
      };
    });
  });

  return entries;
}

/** 按 `(msg, block)` 找条目下标。找不到就退到最近的一条，至少滚到附近。 */
export function indexOfSource(entries: Entry[], src: SourceRef): number {
  let fallback = -1;
  for (let i = 0; i < entries.length; i += 1) {
    const entry = entries[i];
    if (!entry) continue;
    const ref = "src" in entry ? entry.src : undefined;
    if (!ref) continue;
    if (ref.msg === src.msg && ref.block === src.block) return i;
    if (ref.msg === src.msg && (fallback < 0 || ref.block <= src.block)) fallback = i;
  }
  return fallback;
}

/**
 * 在当前会话的内存条目里搜。
 *
 * 与全局搜索的真实差异：这里**能搜到本次运行的思考块**（思考不落盘），
 * 而全局搜索只能搜历史。界面上要把这点写清楚，否则用户会以为是漏搜。
 */
export function searchEntries(
  entries: Entry[],
  query: string,
): Array<{ index: number; snippet: string }> {
  const needle = query.trim().toLowerCase();
  if (!needle) return [];

  const out: Array<{ index: number; snippet: string }> = [];
  entries.forEach((entry, index) => {
    const text =
      entry.kind === "tool"
        ? `${entry.name} ${JSON.stringify(entry.input)} ${entry.output ?? ""}`
        : entry.text;
    const at = text.toLowerCase().indexOf(needle);
    if (at < 0) return;

    const start = Math.max(0, at - 40);
    const body = text
      .slice(start, at + needle.length + 60)
      .replace(/\s+/g, " ")
      .trim();
    out.push({
      index,
      snippet: `${start > 0 ? "…" : ""}${body}${at + needle.length + 60 < text.length ? "…" : ""}`,
    });
  });
  return out;
}
