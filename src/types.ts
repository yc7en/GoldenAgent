/**
 * 后端事件的类型定义。
 *
 * 这份定义必须和 `crates/core/src/event.rs` 的 `Event` 一一对应——
 * 那是跨语言契约，改那边就要改这边（变体名一律 snake_case）。
 */

export type StopReason =
  | "end_turn"
  | "max_turns"
  | "max_tokens"
  | "cancelled"
  | "budget_exceeded"
  | "error";

export type AgentEvent =
  | { type: "turn_started"; turn: number }
  | { type: "text_delta"; text: string }
  /**
   * 推理模型的思考过程。与 `text_delta` 并列：它只用于展示，
   * 不属于回答，也不进入历史（后端不会把它写进消息）。
   */
  | { type: "thinking_delta"; text: string }
  | { type: "tool_started"; id: string; name: string; input: unknown }
  | {
      type: "tool_finished";
      id: string;
      name: string;
      ok: boolean;
      output: string;
    }
  | {
      type: "permission_requested";
      id: string;
      name: string;
      summary: string;
    }
  | { type: "context_compacted"; removed: number }
  | { type: "turn_finished"; turn: number }
  | { type: "finished"; reason: StopReason }
  | { type: "error"; message: string };

export interface Snapshot {
  mode: string;
  provider: string;
  /** 实际会发给厂商的模型名。 */
  model: string;
  workspace: string;
  messages: number;
  pending_permissions: number;
  /** 当前会话 id；新会话还没说过话时是 null。 */
  session_id: string | null;
  /** provider 三态：配好了 / 没配 / 配错了。 */
  provider_state: ProviderStateName;
  /** 仅 degraded 时有值。 */
  provider_reason: string | null;
  /** 应用版本。 */
  version: string;
}

export type ProviderStateName = "ready" | "unconfigured" | "degraded";

export interface ProviderOption {
  id: string;
  /** 该厂商的默认模型名，表单留空时用它。 */
  default_model: string;
  /** 是否需要 API Key（`mock` 不需要）。 */
  needs_key: boolean;
}

/**
 * provider 面板的全部数据，对应 `src-tauri` 的 `ProviderSetupDto`。
 *
 * 注意这里**没有 api_key**：后端不下发密钥，表单里留空表示"不修改"。
 */
export interface ProviderSetup {
  state: ProviderStateName;
  source: "env" | "file" | "none";
  reason: string | null;
  kind: string;
  model: string;
  base_url: string;
  /** 单次回答的输出上限。推理模型的思考也吃这份额度。 */
  max_tokens: number;
  config_path: string;
  /** 环境变量里探测到的 key 变量名（不是值）。 */
  env_key: string | null;
  /** 可直接落盘的配置模板。 */
  template: string;
  catalog: ProviderOption[];
}

export interface AppInfo {
  version: string;
  identifier: string;
  workspace: string;
  /** 工作区怎么定的：显式指定 / 当前目录像项目 / 家目录默认。 */
  workspace_origin: "env" | "cwd" | "home";
  config_path: string;
  /** 没有发布渠道时为 null，界面不显示"检查更新"。 */
  update_url: string | null;
}

export interface ChatReport {
  outcome: "completed" | "max_turns" | "cancelled";
  turns: number;
}

/** 一条历史会话在侧栏里的样子。对应 `crates/session/src/store.rs` 的 `SessionSummary`。 */
export interface SessionSummary {
  id: string;
  title: string;
  messages: number;
  updated_at_ms: number;
}

/**
 * 落盘的消息形态，对应 `crates/core/src/message.rs`。
 *
 * 与事件流是两套东西：事件流是"正在发生什么"，这个是"已经发生过什么"。
 * 打开历史会话时拿到的就是它。
 */
export type ContentBlock =
  | { type: "text"; text: string }
  | { type: "tool_call"; id: string; name: string; input: unknown }
  | { type: "tool_result"; id: string; ok: boolean; output: string };

export interface Message {
  role: "user" | "assistant";
  blocks: ContentBlock[];
}

export const MODE_LABELS: Record<string, string> = {
  read_only: "只读",
  ask: "每次询问",
  workspace_write: "工作区可写",
  bypass: "全放行",
};

/** 厂商 id → 人看的名字。id 由后端 `ProviderKind::as_str` 决定。 */
export const PROVIDER_LABELS: Record<string, string> = {
  mock: "离线回显（不出网）",
  anthropic: "Anthropic",
  openai: "OpenAI 兼容",
};

export const STOP_LABELS: Record<string, string> = {
  end_turn: "模型作答完毕",
  max_turns: "达到轮次上限",
  max_tokens: "达到输出上限",
  cancelled: "已取消",
  budget_exceeded: "超出预算",
  error: "出错了",
};
