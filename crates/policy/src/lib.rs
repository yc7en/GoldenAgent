//! 第 2 层：权限裁决。
//!
//! **全系统只有这一层回答"该不该做"**（A-4）。工具自己不做判断，
//! 循环也不做判断，它只负责拿着 [`Decision`] 去执行或回绝。
//!
//! 判定只认 [`ToolSpec::kind`]，**不认工具名**——新增工具不需要改 policy。
//!
//! 与 jail 的分工：policy 能放行，但放行不等于能越界。
//! 路径是否在工作区外由 `tools::jail` 硬拒绝（A-6），即使 `Mode::Bypass` 也不放开。

use std::path::{Path, PathBuf};

use goldenagent_core::{ToolKind, ToolSpec};
use goldenagent_tools::jail::resolve_in;
use serde_json::Value;

/// 四档权限模式。默认 [`Mode::Ask`]：读写文件都要用户点头。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// 只读：所有写类工具一律拒绝。
    ReadOnly,
    /// 询问：工作区内写入需用户授权，工作区外直接拒绝。
    #[default]
    Ask,
    /// 工作区可写：工作区内写入放行，工作区外拒绝。
    WorkspaceWrite,
    /// 全放行：不做权限判断（路径越界仍由 jail 挡）。
    Bypass,
}

impl Mode {
    pub const ALL: [Mode; 4] = [
        Mode::ReadOnly,
        Mode::Ask,
        Mode::WorkspaceWrite,
        Mode::Bypass,
    ];

    /// 跨语言标识：前端传的就是这个字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::ReadOnly => "read_only",
            Mode::Ask => "ask",
            Mode::WorkspaceWrite => "workspace_write",
            Mode::Bypass => "bypass",
        }
    }

    pub fn parse(raw: &str) -> Option<Mode> {
        Mode::ALL.into_iter().find(|m| m.as_str() == raw)
    }
}

/// 三态裁决。`Ask` 只是"需要问"，是否放行由 `Approver` 决定（A-7）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny { reason: String },
    Ask { summary: String },
}

impl Decision {
    pub fn is_allow(&self) -> bool {
        matches!(self, Decision::Allow)
    }

    pub fn is_deny(&self) -> bool {
        matches!(self, Decision::Deny { .. })
    }
}

#[derive(Debug, Clone)]
pub struct Policy {
    mode: Mode,
    workspace: PathBuf,
    /// 空间附加的目录。空的时候一切与单根时代相同。
    extra_roots: Vec<PathBuf>,
}

impl Policy {
    pub fn new(mode: Mode, workspace: impl Into<PathBuf>) -> Self {
        Self {
            mode,
            workspace: workspace.into(),
            extra_roots: Vec::new(),
        }
    }

    /// `roots[0]` 是主根，其余是附加根。
    ///
    /// 附加根在 `WorkspaceWrite` 档下与主根同等待遇——这是 B18 的决定 2：
    /// 「附加进来但不能写」在权限上等于没附加，用户每写一次点一次授权，
    /// 两次之后就再也不用了。
    pub fn with_roots(mode: Mode, roots: Vec<PathBuf>) -> Self {
        let mut roots = roots;
        let workspace = if roots.is_empty() {
            PathBuf::new()
        } else {
            roots.remove(0)
        };
        Self {
            mode,
            workspace,
            extra_roots: roots,
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// 主根 + 附加根。空工作区不占位——否则它会被当成"空前缀"，
    /// 而任何路径都以空前缀开头，等于处处放行。
    pub fn roots(&self) -> Vec<PathBuf> {
        let mut roots = Vec::with_capacity(1 + self.extra_roots.len());
        if !self.workspace.as_os_str().is_empty() {
            roots.push(self.workspace.clone());
        }
        roots.extend(self.extra_roots.iter().cloned());
        roots
    }

    /// 移动端/前端切档用。返回是否有变化。
    pub fn set_mode(&mut self, mode: Mode) -> bool {
        if self.mode == mode {
            return false;
        }
        self.mode = mode;
        true
    }

    /// 唯一裁决入口。
    ///
    /// 顺序：`Bypass` 全放行 → 读类放行 → 执行类单独判 → 写类按「档位 × 路径是否在工作区内」。
    pub fn decide(&self, spec: &ToolSpec, input: &Value) -> Decision {
        if self.mode == Mode::Bypass {
            return Decision::Allow;
        }

        match spec.kind {
            ToolKind::Read => Decision::Allow,
            ToolKind::Exec => self.decide_exec(spec, input),
            ToolKind::Write => self.decide_write(spec, input),
        }
    }

    /// 执行类工具没有"路径"可判。
    ///
    /// 一条 `rm -rf ~` 的 cwd 就在工作区里，但它干的事和工作区没关系——
    /// 所以「工作区可写」这个档位**不能**替它背书，只有全放行档不问。
    /// 换句话说：`WorkspaceWrite` 与 `Ask` 对执行类是同一个待遇，这是刻意的。
    fn decide_exec(&self, spec: &ToolSpec, input: &Value) -> Decision {
        let command = input
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();

        if self.mode == Mode::ReadOnly {
            return Decision::Deny {
                reason: format!("当前为只读模式，禁止执行命令：{}", brief(command)),
            };
        }

        Decision::Ask {
            summary: format!("{} 想执行命令：{}", spec.name, brief(command)),
        }
    }

    fn decide_write(&self, spec: &ToolSpec, input: &Value) -> Decision {
        let Some(raw_path) = input.get("path").and_then(Value::as_str) else {
            // 写工具不说明写哪儿，没法判，只能拒。
            return Decision::Deny {
                reason: format!("{} 没有提供 path，无法确定写入目标", spec.name),
            };
        };

        let inside = resolve_in(&self.roots(), raw_path).is_ok();

        match (self.mode, inside) {
            (Mode::ReadOnly, _) => Decision::Deny {
                reason: format!("当前为只读模式，禁止 {} 写入 {raw_path}", spec.name),
            },
            (Mode::Ask, true) => Decision::Ask {
                summary: format!("{} 想写入 {raw_path}", spec.name),
            },
            (Mode::Ask, false) => Decision::Deny {
                reason: format!("{raw_path} 不在允许的目录里，Ask 模式不允许写工作区之外"),
            },
            (Mode::WorkspaceWrite, true) => Decision::Allow,
            (Mode::WorkspaceWrite, false) => Decision::Deny {
                reason: format!("{raw_path} 不在允许的目录里，工作区可写模式只覆盖空间内（主目录 + 附加目录）"),
            },
            (Mode::Bypass, _) => Decision::Allow,
        }
    }
}

/// 授权条上放不下整条命令，但也不能截到看不出在干什么。
fn brief(command: &str) -> String {
    const LIMIT: usize = 120;

    let first_line = command.lines().next().unwrap_or("").trim();
    if first_line.chars().count() <= LIMIT {
        return first_line.to_string();
    }
    let head: String = first_line.chars().take(LIMIT).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use goldenagent_core::ToolKind;
    use serde_json::json;

    fn spec(name: &str, kind: ToolKind) -> ToolSpec {
        ToolSpec::new(name, "", json!({}), kind)
    }

    fn policy(mode: Mode) -> Policy {
        Policy::new(mode, "/work/project")
    }

    fn read_input() -> Value {
        json!({"path": "src/main.rs"})
    }

    fn write_inside() -> Value {
        json!({"path": "out/result.txt"})
    }

    fn write_outside() -> Value {
        json!({"path": "../outside.txt"})
    }

    fn exec_input() -> Value {
        json!({ "command": "ls -la" })
    }

    fn decide(mode: Mode, kind: ToolKind, input: Value) -> Decision {
        policy(mode).decide(&spec("whatever", kind), &input)
    }

    /// 主根 + 一个附加根。用 temp_dir 拼，避免 `/data/...` 在 Windows 上
    /// 被判成相对路径（那边没有盘符就不算绝对路径）。
    fn roots_pair() -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir();
        (base.join("ga-b18-primary"), base.join("ga-b18-extra"))
    }

    /// 附加根在 `WorkspaceWrite` 档下必须与主根同等待遇。
    ///
    /// 这是 B18 的决定 2。反过来说：如果哪天有人为了"更安全"把附加根
    /// 从 `WorkspaceWrite` 的放行范围里摘出去，这条会红——那等于让这个功能
    /// 只在 `Bypass` 档可用，而 `Bypass` 本来就不需要它。
    #[test]
    fn workspace_write_covers_extra_roots() {
        let (primary, extra) = roots_pair();
        let inside_extra = json!({ "path": extra.join("spec.md").to_string_lossy() });
        let spec_ = spec("write_file", ToolKind::Write);

        let ws = Policy::with_roots(Mode::WorkspaceWrite, vec![primary.clone(), extra.clone()]);
        assert_eq!(ws.decide(&spec_, &inside_extra), Decision::Allow);

        let ask = Policy::with_roots(Mode::Ask, vec![primary.clone(), extra.clone()]);
        assert!(matches!(ask.decide(&spec_, &inside_extra), Decision::Ask { .. }));

        let ro = Policy::with_roots(Mode::ReadOnly, vec![primary, extra]);
        assert!(ro.decide(&spec_, &inside_extra).is_deny());
    }

    /// 加了附加根，不等于把工作区外也放开了。
    #[test]
    fn extra_roots_do_not_widen_the_boundary_beyond_the_list() {
        let (primary, extra) = roots_pair();
        let outside = json!({ "path": extra.join("..").join("other").join("x.md").to_string_lossy() });

        let p = Policy::with_roots(Mode::WorkspaceWrite, vec![primary, extra]);
        assert!(p
            .decide(&spec("write_file", ToolKind::Write), &outside)
            .is_deny());
    }

    /// 空根集合不能变成"处处放行"。
    #[test]
    fn empty_roots_deny_rather_than_allow() {
        let p = Policy::with_roots(Mode::WorkspaceWrite, Vec::new());
        assert!(p
            .decide(&spec("write_file", ToolKind::Write), &json!({"path": "a.txt"}))
            .is_deny());
    }

    /// 四档 × 读/写 × 内/外 的完整矩阵。
    #[test]
    fn mode_matrix_is_exhaustive() {
        use Decision::*;

        // 读：任何档位都放行（越界由 jail 兜底，不属于权限问题）
        for mode in Mode::ALL {
            assert_eq!(decide(mode, ToolKind::Read, read_input()), Allow, "{mode:?} 读");
            assert_eq!(
                decide(mode, ToolKind::Read, write_outside()),
                Allow,
                "{mode:?} 读越界由 jail 管"
            );
        }

        // 写：按档位判定
        assert!(decide(Mode::ReadOnly, ToolKind::Write, write_inside()).is_deny());
        assert!(decide(Mode::ReadOnly, ToolKind::Write, write_outside()).is_deny());

        assert!(matches!(
            decide(Mode::Ask, ToolKind::Write, write_inside()),
            Ask { .. }
        ));
        assert!(decide(Mode::Ask, ToolKind::Write, write_outside()).is_deny());

        assert_eq!(
            decide(Mode::WorkspaceWrite, ToolKind::Write, write_inside()),
            Allow
        );
        assert!(decide(Mode::WorkspaceWrite, ToolKind::Write, write_outside()).is_deny());

        assert_eq!(decide(Mode::Bypass, ToolKind::Write, write_inside()), Allow);
        assert_eq!(decide(Mode::Bypass, ToolKind::Write, write_outside()), Allow);
    }

    /// 执行类的完整矩阵。
    ///
    /// 关键一条：`WorkspaceWrite` 对执行类**不是**放行。这个档位承诺的是
    /// "工作区内的写入不用问"，而一条 shell 命令能做的事远在工作区之外。
    #[test]
    fn exec_matrix_is_exhaustive() {
        use Decision::*;
        let cmd = exec_input();

        assert!(decide(Mode::ReadOnly, ToolKind::Exec, cmd.clone()).is_deny());

        for mode in [Mode::Ask, Mode::WorkspaceWrite] {
            assert!(
                matches!(decide(mode, ToolKind::Exec, cmd.clone()), Ask { .. }),
                "{mode:?} 下执行命令必须询问"
            );
        }

        assert_eq!(decide(Mode::Bypass, ToolKind::Exec, cmd), Allow);
    }

    #[test]
    fn exec_summary_is_single_line_and_bounded() {
        let long = format!("echo {}\necho 再来一行", "x".repeat(400));
        let Decision::Ask { summary } =
            decide(Mode::Ask, ToolKind::Exec, json!({ "command": long }))
        else {
            panic!("应为询问");
        };
        assert!(summary.ends_with('…'), "{summary}");
        assert!(!summary.contains('\n'), "授权条放不下多行");
        assert!(summary.chars().count() < 160, "{}", summary.chars().count());
    }

    /// 不硬编码工具名：随便换个名字、同样的 kind，行为必须一致。
    #[test]
    fn does_not_hardcode_tool_names() {
        let p = policy(Mode::WorkspaceWrite);
        for name in ["write_file", "apply_patch", "frobnicate"] {
            assert_eq!(
                p.decide(&spec(name, ToolKind::Write), &write_inside()),
                Decision::Allow,
                "{name} 应被同一套规则处理"
            );
        }
    }

    #[test]
    fn write_without_path_is_denied() {
        let decision = decide(Mode::Bypass, ToolKind::Write, json!({}));
        // Bypass 先返回 Allow，所以这里要用非 Bypass 档验证。
        assert_eq!(decision, Decision::Allow);

        let decision = decide(Mode::WorkspaceWrite, ToolKind::Write, json!({"content": "x"}));
        assert!(decision.is_deny());
    }

    #[test]
    fn deny_reason_mentions_path() {
        let Decision::Deny { reason } = decide(Mode::Ask, ToolKind::Write, write_outside()) else {
            panic!("应为拒绝");
        };
        assert!(reason.contains(".."), "{reason}");
    }

    #[test]
    fn ask_summary_mentions_tool_and_path() {
        let Decision::Ask { summary } = policy(Mode::Ask).decide(
            &spec("write_file", ToolKind::Write),
            &json!({"path": "a.txt"}),
        ) else {
            panic!("应为询问");
        };
        assert!(summary.contains("write_file"), "{summary}");
        assert!(summary.contains("a.txt"), "{summary}");
    }

    #[test]
    fn mode_roundtrips_through_string() {
        for mode in Mode::ALL {
            assert_eq!(Mode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(Mode::parse("nope"), None);
    }

    #[test]
    fn default_mode_is_ask() {
        assert_eq!(Mode::default(), Mode::Ask);
        assert!(!policy(Mode::Ask).mode().as_str().is_empty());
    }
}
