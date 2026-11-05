//! B19 · 技能（只读版）的验收。
//!
//! 这一层要证明的三件事：
//! 1. 放一个目录进去，`use_skill` 就能读到正文（含 references）；
//! 2. 读不到技能目录之外的任何东西——包括同一个 skills 根下的兄弟目录；
//! 3. 出错时说得清「为什么」和「现有的有哪些」，而不是一句"失败"。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use goldenagent_tools::{Registry, ToolContext};
use serde_json::json;

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct Temp {
    path: PathBuf,
}

impl Temp {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "goldenagent-skill-it-{}-{stamp}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn write(&self, relative: &str, content: &str) {
        let path = self.path.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    /// 工作区与技能根刻意分开：技能不是工作区，混在一起就分不清是不是越界了。
    fn ctx(&self) -> ToolContext {
        ToolContext::new(self.path.join("workspace")).with_skills_root(&self.path)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn registry() -> Registry {
    Registry::with_defaults()
}

const SKILL: &str = "---\nname: PDF 处理\ndescription: 处理 PDF 时用\n---\n第一步：提取文本\n第二步：核对表格\n";

#[tokio::test]
async fn use_skill_reads_the_body_with_line_numbers() {
    let temp = Temp::new();
    temp.write("pdf/SKILL.md", SKILL);

    let out = registry()
        .run("use_skill", json!({"name": "pdf"}), &temp.ctx())
        .await
        .unwrap();

    assert!(out.contains("技能 pdf · SKILL.md · 共 6 行"), "{out}");
    assert!(out.contains("第一步：提取文本"), "{out}");
    assert!(out.contains("     6\t第二步：核对表格"), "要带行号: {out}");
}

#[tokio::test]
async fn use_skill_reads_referenced_files() {
    let temp = Temp::new();
    temp.write("pdf/SKILL.md", SKILL);
    temp.write("pdf/references/表格约定.md", "合并单元格要单独核对\n");

    let out = registry()
        .run(
            "use_skill",
            json!({"name": "pdf", "path": "references/表格约定.md"}),
            &temp.ctx(),
        )
        .await
        .unwrap();

    assert!(out.contains("合并单元格要单独核对"), "{out}");
}

#[tokio::test]
async fn use_skill_lists_a_directory_when_asked() {
    let temp = Temp::new();
    temp.write("pdf/SKILL.md", SKILL);
    temp.write("pdf/references/a.md", "x");
    temp.write("pdf/references/b.md", "y");

    let out = registry()
        .run("use_skill", json!({"name": "pdf", "path": "references"}), &temp.ctx())
        .await
        .unwrap();

    assert!(out.contains("是目录"), "{out}");
    assert!(out.contains("a.md") && out.contains("b.md"), "{out}");
}

/// 破坏性验收的核心：宁可失败，也不能读到技能目录之外。
#[tokio::test]
async fn use_skill_cannot_read_outside_the_skill_dir() {
    let temp = Temp::new();
    temp.write("pdf/SKILL.md", SKILL);
    temp.write("secret.txt", "API_KEY=泄露了就完了");
    temp.write("other/SKILL.md", "---\ndescription: 兄弟技能\n---\n");

    let reg = registry();
    let ctx = temp.ctx();

    // 1) `..` 逃到 skills 根
    let err = reg
        .run("use_skill", json!({"name": "pdf", "path": "../secret.txt"}), &ctx)
        .await
        .expect_err("必须拒绝");
    assert!(err.to_string().contains("不允许"), "{err}");

    // 2) 同根下的兄弟技能也不能读
    let err = reg
        .run("use_skill", json!({"name": "pdf", "path": "../other/SKILL.md"}), &ctx)
        .await
        .expect_err("必须拒绝");
    assert!(err.to_string().contains("不允许"), "{err}");

    // 3) 绝对路径更不能读
    let absolute = temp.path.join("secret.txt").display().to_string();
    let err = reg
        .run("use_skill", json!({"name": "pdf", "path": absolute}), &ctx)
        .await
        .expect_err("必须拒绝");
    assert!(err.to_string().contains("不允许"), "{err}");

    // 4) 名字本身也不能穿越
    let err = reg
        .run("use_skill", json!({"name": "../other"}), &ctx)
        .await
        .expect_err("必须拒绝");
    assert!(err.to_string().contains("不合法"), "{err}");
}

#[tokio::test]
async fn use_skill_says_what_exists_when_the_name_is_wrong() {
    let temp = Temp::new();
    temp.write("pdf/SKILL.md", SKILL);
    temp.write("docx/SKILL.md", "---\ndescription: 处理 Word\n---\n");

    let err = registry()
        .run("use_skill", json!({"name": "pptx"}), &temp.ctx())
        .await
        .expect_err("名字不对必须报错");

    let text = err.to_string();
    assert!(text.contains("没有名为 pptx"), "{text}");
    assert!(text.contains("docx") && text.contains("pdf"), "要列出可用的: {text}");
}

#[tokio::test]
async fn use_skill_without_a_skills_root_says_so() {
    let temp = Temp::new();
    // 不给 with_skills_root：这台机器没启用技能
    let ctx = ToolContext::new(&temp.path);

    let err = registry()
        .run("use_skill", json!({"name": "pdf"}), &ctx)
        .await
        .expect_err("没有技能根时必须报错");

    assert!(err.to_string().contains("没有可用的技能目录"), "{err}");
}

#[tokio::test]
async fn use_skill_reports_an_empty_root_honestly() {
    let temp = Temp::new();
    let err = registry()
        .run("use_skill", json!({"name": "pdf"}), &temp.ctx())
        .await
        .expect_err("空目录里也没有技能");

    assert!(err.to_string().contains("一个技能也没有"), "{err}");
}

#[tokio::test]
async fn use_skill_is_a_read_tool() {
    // 读类工具在 ReadOnly 档也放行：技能只是文本，读它不该要授权。
    assert_eq!(
        registry().spec("use_skill").unwrap().kind,
        goldenagent_core::ToolKind::Read
    );
}

/// 技能文件可能是从别处拷来的 GBK 文本，不能因此读成乱码。
#[cfg(windows)]
#[tokio::test]
async fn use_skill_decodes_non_utf8_skill_files() {
    let temp = Temp::new();
    let dir = temp.path.join("gbk");
    std::fs::create_dir_all(&dir).unwrap();
    // "---\ndescription: 中文说明\n---\n" 的 GBK 编码
    let mut bytes = b"---\ndescription: ".to_vec();
    bytes.extend_from_slice(&[0xD6, 0xD0, 0xCE, 0xC4, 0xCB, 0xB5, 0xC3, 0xF7]);
    bytes.extend_from_slice(b"\n---\n");
    std::fs::write(dir.join("SKILL.md"), bytes).unwrap();

    let out = registry()
        .run("use_skill", json!({"name": "gbk"}), &temp.ctx())
        .await
        .unwrap();

    assert!(out.contains("中文说明"), "{out}");
}
