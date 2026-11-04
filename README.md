# GoldenAgent

本地优先的桌面 AI Agent 宿主。Rust 核心 + Tauri 2 外壳 + React 前端。

一期目标是**把闭环跑通**：输入一句话 → 循环问模型 → 调工具 → 结果回灌 → 事件流回前端。
不追求功能矩阵，追求"每一层都能被单独替换、单独测试"。

---

## 目录结构

```
goldenagent/
├── crates/
│   ├── core/        第 0 层  类型 + Event 契约 + Error
│   ├── provider/    第 1 层  模型接入（离线 Mock / Anthropic / OpenAI 兼容）
│   ├── tools/       第 1 层  工具实现 + 路径 jail
│   ├── session/     第 1 层  会话、token 估算、压缩、JSONL 持久化
│   ├── policy/      第 2 层  权限裁决（四档）
│   ├── agent-loop/  第 3 层  唯一的 agent 循环 → 事件流
│   └── app/         第 4 层  装配、授权往返、会话生命周期
├── src-tauri/       第 5 层  薄壳：command 转发 + event 转发（唯一允许 use tauri）
├── src/                       React + TS 前端
└── scripts/                  分层门禁、版本一致性、体积预算、图标生成（全部零依赖 Node）
```

**依赖只能自上而下**。`crates/*` 里出现 `tauri` 就是架构事故——核心一旦认识
`AppHandle`，就再也没法脱离窗口跑测试、也没法挪去做 CLI。

---

## 快速开始

```bash
# 1. 分层门禁（秒级，不需要编译，改完代码先跑这个）
npm run check

# 2. 核心层测试（不开窗口、不需要 WebView）
cargo test --workspace

# 3. 桌面应用
npm install
npm run tauri dev

# 1+2+前端构建一把梭
npm run verify
```

> 开发端口是 **5320**，不是 Tauri 文档里常见的 1420：1420/1421 在本机落在 Windows 的
> TCP 保留端口区间内，`listen` 直接 `EACCES`。`vite.config.ts` 用 `strictPort: true`，
> 端口被占会直接失败而不是悄悄换一个（换端口会让 Tauri 白屏）。

不配任何东西也能跑：`MockProvider` 会把你发送的内容原样回显，但**工具是真的**——
说"看看有哪些文件"，它真的会 `list_dir`；说"写个文件"，`Ask` 档下会弹授权条。

### 接真实模型

三种方式，任选其一。**桌面用户直接用第一种**。

**方式零：界面里填**（推荐）

顶栏「设置」→ 选厂商 → 填 API Key → 保存。保存即生效，不用重启。
配置会写到 `.goldenagent/provider.json`，和下面的手写文件是同一份。

没配过时，对话区会直接给一张「未配置模型」的卡片和一个按钮，不会只丢一句"离线回显"——
"没配"和"配错了"在界面上是两种不同的提示。

**方式一：环境变量**（命令行启动、CI、临时试；优先级最高，会盖过配置文件）

```bash
# Anthropic
export ANTHROPIC_API_KEY=sk-ant-...
npm run tauri dev
```

```bash
# OpenAI 兼容：DeepSeek / Moonshot / vLLM / Ollama 的 /v1 / 自建网关都走这条
export OPENAI_API_KEY=sk-...
export GOLDENAGENT_PROVIDER=openai
export GOLDENAGENT_MODEL=deepseek-chat
export GOLDENAGENT_BASE_URL=https://api.deepseek.com/v1
npm run tauri dev
```

**方式二：配置文件**（**桌面双击启动必须用这个**——从资源管理器起的进程读不到你
shell 里 `export` 的变量）

```bash
cp .goldenagent.example/provider.json .goldenagent/provider.json
# 然后填 api_key
```

```json
{
  "kind": "anthropic",
  "model": "claude-sonnet-4-5",
  "api_key": "sk-ant-...",
  "base_url": null,
  "timeout_secs": 180,
  "max_retries": 2,
  "max_tokens": 8192
}
```

| 字段 | 说明 |
|---|---|
| `kind` | `anthropic` / `openai` / `mock` |
| `model` | 留空用默认（`claude-sonnet-4-5` / `gpt-4o-mini`） |
| `api_key` | 为空会明确报错，不会静默失败 |
| `base_url` | 自建网关或本地模型才填。**Anthropic 填到域名为止；OpenAI 兼容要带 `/v1`** |
| `timeout_secs` | 默认 180。长回答超时的话调大 |
| `max_retries` | 429 / 5xx / 网络抖动的重试次数，默认 2。设 0 关闭 |
| `max_tokens` | 单次回答的输出上限，默认 8192。**推理模型的思考也算在里面**，见下方说明 |
| `stream` | 默认 `true`。极少数网关声称兼容却不支持 SSE，填 `false` 退回整段返回（功能一样，只是没有逐字效果） |

### 推理模型（`deepseek-*` / `*-thinking` 等）

这类模型先输出一段**思考**再给正文，思考也占用 `max_tokens`。额度被思考吃光时，
正文一个字都不会有，你会看到：

> 这一轮把 256 tokens 的额度全用在思考上了（已思考 770 字，正文还没开始就结束）。

这不是网关坏了，把 `max_tokens` 调大即可。思考过程会实时显示成一块可折叠的
「思考中…」，它**不进入对话历史**，也不会计入下一轮请求。

优先级：**环境变量 > 配置文件 > 离线回显**。

`.goldenagent/` 已进 `.gitignore`（里面有 key 和完整对话记录），不会被误提交。

配错了不会白屏也不会崩：降级到离线回显，并且**把原因显示在顶栏**——
静默降级比直接报错更让人摸不着头脑，所以这条是刻意做的。

---

## 分层规则（不是建议，是门禁）

| crate | 层 | 允许依赖 |
|---|---|---|
| `goldenagent-core` | 0 | — |
| `goldenagent-provider` | 1 | core |
| `goldenagent-tools` | 1 | core |
| `goldenagent-session` | 1 | core |
| `goldenagent-policy` | 2 | core、tools |
| `goldenagent-loop` | 3 | 上面全部 |
| `goldenagent-app` | 4 | 上面全部 |
| `src-tauri` | 5 | 仅 `app` + tauri |

同层之间也不许互相依赖（`provider` 不许依赖 `tools`）——同层依赖是分层最先烂掉的地方。

这些规则由 `npm run check` 强制执行，共五条：

| 规则 | 内容 |
|---|---|
| R1 | 内部依赖必须严格向下，同层互依赖也算违规 |
| R2 | `crates/**` 不得出现 `tauri`（注释与字符串先剥离，不误报） |
| R3 | 新增 crate 必须先在依赖矩阵登记层级，否则失败 |
| R4 | 依赖不得成环 |
| R5 | 外壳只能依赖 `app`，不许绕过它去够内部层 |

脚本零依赖，`node` 直接跑，不需要 `cargo`，所以前端同学也能在提交前自查。
**加 crate 的顺序是：先改矩阵 → 再改脚本的 `LAYERS` 表 → 最后写代码**，反过来就等于让矩阵迁就代码。

---

## 三条加功能的正确姿势

**加一个工具**：在 `crates/tools/src/` 写一个 `impl Tool`，在 `Registry::with_defaults()`
里 `register`。循环、policy、前端都不用改。`ToolSpec.kind` 决定它走读还是写规则。

**换一个模型**：在 `crates/provider/` 实现 `Provider`，把 `AgentService::offline()`
换成 `AgentService::new(config, Arc::new(YourProvider))`。循环和事件契约不变。
只实现 `complete` 也能跑——`stream` 有默认实现，会退化成"整段一次发"；
想真流式就再实现 `stream`，推 `StreamChunk` 即可。

**加一种进度**：在 `core::Event` 加变体 → 前端 `src/types.ts` 同步加 → 循环里 `emit`。
不要加回调、不要让前端去读后端内部字段。

---

## 两处刻意不做的事

- **不开本地 HTTP 端口**。Tauri IPC 已经够用；再开一个端口意味着两套接口、两份鉴权、两个攻击面。
- **不做第二套引擎**。`crates/agent-loop` 是唯一的循环实现，加功能只能加在它里面。

---

## 打包与发布

```bash
npm run tauri build     # 产出主程序 + 安装包（Windows 是 NSIS）
npm run bundle:size     # 体积对照预算：主程序 20 MiB / 安装包 10 MiB
npm run check:version   # 版本号三处一致性（package.json / tauri.conf.json / Cargo.toml）
```

- **Windows 走 NSIS 按用户安装**（`installMode: currentUser`），不弹 UAC、装到用户目录。
- 打包目标按平台收窄：Windows `nsis` / macOS `app,dmg` / Linux `deb,appimage`。
- 图标是生成出来的（`npm run icon`），png/ico/icns 三套齐全，不往仓库塞二进制。
- **不做自动更新**。`GOLDENAGENT_UPDATE_URL` 有值时界面才显示「检查更新」，没配就不显示。

---

## 当前进度

B0–B17 全部完成。能力范围：

- 分层骨架 + 五条门禁（R1–R5）+ 版本一致性检查
- provider：离线回显 / Anthropic / OpenAI 兼容，SSE 流式，退避重试，**三态装配状态**
- 工具七个：`read_file` `list_dir` `grep` `glob` `write_file` `apply_patch` `bash`
- 四档权限，`Exec` 类单独定档，授权条带超时拒绝
- 会话：内存 + 多会话 `sessions/<space>/<id>.jsonl`，可列可恢复可改名；置顶 / 归档 / 全局搜索
- **空间**：可切换的活动工作区，会话按空间分目录，工具与授权按空间的目录走
- 上下文压缩、取消、端到端场景回归（12 个场景断言完整事件序列）
- 安装包产出、界面内配 provider、体积预算、工作区落点（不会把安装目录当项目）

验证状态：

- `npm run verify` —— 退出码 0（门禁 R1–R5 + 版本一致性 + 全量测试 + 前端构建）
- `cargo test --workspace` —— **240 用例全过，无代码警告**（Windows 的增量编译锁文件
  警告是环境噪音，不计入）
- `npm run tauri build` —— 通过，产出 NSIS 安装包
- `npm run bundle:size` —— 在预算内（B17 未重测，前端只涨了静态资源）

> 判断一次改动是改进还是退步，以及每次改动要交哪些证据，见 [`评价标准.md`](评价标准.md)。

> **仍需人工确认的**：①对话 / 授权 / 切档 / 会话侧栏的界面交互；②流式的真实观感
> （字是不是一点点出来）；③首次启动"未配置 → 面板填 key → 对话"的完整链路；
> ④在干净机器上装一次；⑤在空间里让模型写一个文件，去那个目录确认文件真的落在那里。
> `cargo test` 能证明服务层行为，证明不了界面那一步真的走得通。
>
> 真实 provider 的验证是**协议级**的：用本地假 HTTP 服务断言"发出去的 body 长相"，
> 没有和真厂商握过手（本机无 key）。第一次填真 key 时请留意 `R-261102-0004.md §7`。
