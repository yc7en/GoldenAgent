import { useEffect, useRef } from "react";

import type { Entry } from "../lib/entries";
import {
  ArrowPathIcon,
  CheckCircleIcon,
  ChevronRightIcon,
  LightBulbIcon,
  ToolIcon,
  XCircleIcon,
} from "./Icons";

/**
 * 折叠状态下那一行摘要，按「最能说明这一步在干什么」的顺序找字段。
 */
const DETAIL_KEYS = ["command", "input", "path", "file_path", "pattern", "query", "url", "name", "prompt", "role", "task_id", "terminal_id", "action"];

function oneLine(text: string): string {
  const flat = text.replace(/\s+/g, " ").trim();
  return flat.length > 90 ? `${flat.slice(0, 90)}…` : flat;
}

function brief(input: unknown): string {
  if (typeof input === "string") return oneLine(input);
  if (input && typeof input === "object") {
    const fields = input as Record<string, unknown>;
    for (const key of DETAIL_KEYS) {
      const value = fields[key];
      if (typeof value === "string" && value.trim()) return oneLine(value);
    }
    if (Array.isArray(fields.edits)) return `${fields.edits.length} 处修改`;
  }
  return "";
}

/**
 * 获取工具的人类友好意图短语与操作参数细节。
 * 优先取模型输出的 summary 意图；若模型未提供，则自动转换为自然的中文动词短语。
 */
function getToolDisplay(name: string, input: unknown): { summary: string; detail?: string } {
  let explicitSummary = "";

  if (input && typeof input === "object") {
    const fields = input as Record<string, unknown>;
    if (typeof fields.summary === "string" && fields.summary.trim()) {
      explicitSummary = fields.summary.trim();
    }
  }

  const detail = brief(input);

  if (explicitSummary) {
    return { summary: explicitSummary, detail: detail || undefined };
  }

  // 降级：当模型未给 summary 时，自动转换成可读的动作短语
  switch (name) {
    case "read_file":
      return { summary: detail ? `阅读 ${detail}` : "读取文件", detail };
    case "write_file":
      return { summary: detail ? `写入 ${detail}` : "写入文件", detail };
    case "apply_patch":
      return { summary: detail ? `修改 ${detail}` : "应用补丁修改", detail };
    case "list_dir":
      return { summary: detail ? `查看目录 ${detail}` : "列出目录子项", detail };
    case "grep":
      return { summary: detail ? `搜索文本 "${detail}"` : "按内容搜索文本", detail };
    case "glob":
      return { summary: detail ? `查找文件 "${detail}"` : "通配查找文件", detail };
    case "bash":
      return { summary: detail ? `运行命令 ${detail}` : "运行 Shell 命令", detail };
    case "use_skill":
      return { summary: detail ? `查阅技能 ${detail}` : "查阅技能文档", detail };
    case "invoke_subagent":
      return { summary: detail ? `委派子代理 ${detail}` : "委派只读子代理调研", detail };
    case "manage_task":
      return { summary: detail ? `管理后台作业 ${detail}` : "管理后台作业", detail };
    case "terminal_create":
      return { summary: detail ? `创建终端 ${detail}` : "创建有状态终端", detail };
    case "terminal_write":
      return { summary: detail ? `输入 ${detail}` : "向终端写入指令", detail };
    case "terminal_read":
      return { summary: detail ? `读取 ${detail}` : "读取终端屏幕增量", detail };
    case "terminal_kill":
      return { summary: detail ? `关闭终端 ${detail}` : "终止终端会话", detail };
    case "inspect_spill":
      return { summary: detail ? `查阅溢出日志 ${detail}` : "查阅大输出溢出日志", detail };
    case "lsp_definition":
      return { summary: detail ? `跳转定义 ${detail}` : "跳转符号定义", detail };
    case "lsp_references":
      return { summary: detail ? `查找引用 ${detail}` : "查找符号全项目引用", detail };
    case "lsp_diagnostics":
      return { summary: detail ? `检查诊断 ${detail}` : "查看实时编译诊断", detail };
    case "lsp_rename":
      return { summary: detail ? `重命名符号 ${detail}` : "安全重命名符号", detail };
    default:
      return { summary: `${name} ${detail}`.trim(), detail };
  }
}

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
        const flash = highlight?.index === index ? " flash-hit" : "";

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
          const streaming = index === entries.length - 1 && busy;
          return (
            <div className={`row thinking${flash}`} key={index} data-entry={index}>
              <details className="thinking-box">
                <summary className="thinking-head">
                  <span className="thinking-icon">
                    <LightBulbIcon className="icon-svg" />
                  </span>
                  <span className="thinking-label">
                    {streaming ? "深度思考中…" : "深度思考"}
                  </span>
                  <span className="thinking-meta">{entry.text.length} 字</span>
                  <span className="thinking-arrow">
                    <ChevronRightIcon className="icon-svg" />
                  </span>
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

        const display = getToolDisplay(entry.name, entry.input);

        return (
          <div className={`tool ${entry.status}${flash}`} key={entry.id} data-entry={index}>
            <details className="tool-box">
              <summary className="tool-head">
                <span className="tool-type-icon">
                  <ToolIcon name={entry.name} className="icon-svg" />
                </span>
                <span className="tool-summary-text" title={display.summary}>
                  {display.summary}
                </span>
                {display.detail && display.summary !== display.detail && (
                  <span className="tool-detail-pill" title={display.detail}>
                    {display.detail}
                  </span>
                )}
                <span className="tool-status-badge">
                  {entry.status === "running" ? (
                    <>
                      <ArrowPathIcon className="icon-svg status-icon spin" />
                      <span className="status-label">执行中</span>
                    </>
                  ) : entry.status === "ok" ? (
                    <>
                      <CheckCircleIcon className="icon-svg status-icon icon-ok" />
                      <span className="status-label">完成</span>
                    </>
                  ) : (
                    <>
                      <XCircleIcon className="icon-svg status-icon icon-failed" />
                      <span className="status-label">失败</span>
                    </>
                  )}
                </span>
                <span className="tool-expand-arrow">
                  <ChevronRightIcon className="icon-svg" />
                </span>
              </summary>
              <div className="tool-body">
                <div className="tool-body-label">调用参数：</div>
                <pre className="tool-input">{JSON.stringify(entry.input, null, 2)}</pre>
                {entry.output !== undefined && (
                  <>
                    <div className="tool-body-label">执行输出：</div>
                    <pre className="tool-output">{entry.output}</pre>
                  </>
                )}
              </div>
            </details>
          </div>
        );
      })}
    </div>
  );
}
