#!/usr/bin/env node
/**
 * 分层门禁 —— 把 ARCHITECTURE.md §6 的依赖矩阵变成可执行的检查。
 *
 * 这个脚本不认识的只有两件事：TOML 和 Rust 的词法。所以它对两者各做了一个
 * 只够用的最小解析（剥离注释、剥离字符串字面量），不做完整语法分析。
 * 目标是"零依赖、任何环境都能跑"，不是"TOML 解析器"。
 *
 * 五条规则：
 *   R1 内部依赖必须严格向下（dep.rank < self.rank），同层互依赖也算违规
 *   R2 crates/** 不得出现 tauri（代码与清单都算）
 *   R3 新增 crate 必须先在 ARCHITECTURE.md §6 登记层级，未登记即违规
 *   R4 内部依赖不得成环（编译器也挡，但这里报得更清楚）
 *   R5 外壳只认 app：goldenagent-shell 不得直接依赖 app 以下的任何 crate
 *
 * 退出码：0 通过，1 有违规。接进 `npm run check`。
 */

import { readFileSync, readdirSync, statSync, existsSync } from "node:fs";
import { join, relative, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");

/**
 * 层级矩阵。唯一真源是 docs-local/GoldenAgent/ARCHITECTURE.md §6。
 * 改这里必须同时改文档，反之亦然——R3 会挡住"悄悄加 crate"。
 */
const LAYERS = {
  "goldenagent-core": 0,
  "goldenagent-provider": 1,
  "goldenagent-tools": 1,
  "goldenagent-session": 1,
  "goldenagent-policy": 2,
  "goldenagent-loop": 3,
  "goldenagent-app": 4,
  "goldenagent-shell": 5,
};

/** 唯一允许出现 tauri 的 crate（R2 的例外）。 */
const TAURI_ALLOWED = new Set(["goldenagent-shell"]);

/** R5：这个 crate 只许依赖它列出的内部 crate。 */
const ONLY_ALLOWED = {
  "goldenagent-shell": new Set(["goldenagent-app"]),
};

const TIER_NAME = ["域模型", "能力层", "裁决层", "循环层", "装配层", "外壳层"];

// ───────────────────────────── 输出 ─────────────────────────────

const useColor = process.stdout.isTTY && !process.env.NO_COLOR;
const c = (code, s) => (useColor ? `\x1b[${code}m${s}\x1b[0m` : s);
const red = (s) => c(31, s);
const green = (s) => c(32, s);
const yellow = (s) => c(33, s);
const dim = (s) => c(2, s);
const bold = (s) => c(1, s);

function section(title) {
  console.log("");
  console.log(bold(title));
  console.log(dim("─".repeat(72)));
}

// ───────────────────────── 最小 TOML 解析 ─────────────────────────

/** 去掉一行里引号之外的 `#` 之后的内容。 */
function stripTomlComment(line) {
  let inStr = false;
  let esc = false;
  for (let i = 0; i < line.length; i++) {
    const ch = line[i];
    if (esc) {
      esc = false;
      continue;
    }
    if (inStr && ch === "\\") {
      esc = true;
      continue;
    }
    if (ch === '"') {
      inStr = !inStr;
      continue;
    }
    if (ch === "#" && !inStr) return line.slice(0, i);
  }
  return line;
}

function balanced(s) {
  let n = 0;
  for (const ch of s) {
    if (ch === "{") n++;
    else if (ch === "}") n--;
  }
  return n <= 0;
}

/**
 * 返回 Map<段名, { line, entries: Map<键, { value, line }> }>。
 * 只支持本项目用到的写法：`[a.b]`、`[[bin]]`、`key = value`、跨行 inline table。
 */
function parseToml(text) {
  const sections = new Map();
  const get = (name) => {
    if (!sections.has(name)) sections.set(name, { line: 0, entries: new Map() });
    return sections.get(name);
  };
  get("");

  let cur = "";
  let pending = null;

  text.split(/\r?\n/).forEach((raw, idx) => {
    const lineNo = idx + 1;
    const line = stripTomlComment(raw);

    if (pending) {
      pending.buf += " " + line.trim();
      if (balanced(pending.buf)) {
        get(pending.section).entries.set(pending.key, { value: pending.buf, line: pending.line });
        pending = null;
      }
      return;
    }

    const s = line.trim();
    if (!s) return;

    const arrSec = s.match(/^\[\[\s*([^\]]+?)\s*\]\]$/);
    if (arrSec) {
      const name = `[[${arrSec[1]}]]@${lineNo}`;
      get(name).line = lineNo;
      cur = name;
      return;
    }
    const sec = s.match(/^\[\s*([^\]]+?)\s*\]$/);
    if (sec) {
      cur = sec[1];
      get(cur).line = lineNo;
      return;
    }

    const kv = s.match(/^([A-Za-z0-9_\-."']+)\s*=\s*(.*)$/);
    if (!kv) return;
    const key = kv[1].replace(/^["']|["']$/g, "");
    const value = kv[2].trim();
    if (value.includes("{") && !balanced(value)) {
      pending = { section: cur, key, line: lineNo, buf: value };
      return;
    }
    get(cur).entries.set(key, { value, line: lineNo });
  });

  return sections;
}

const isDepSection = (name) =>
  /^(?:target\..+\.)?(?:dependencies|dev-dependencies|build-dependencies)$/.test(name);

// ─────────────────────── 最小 Rust 词法剥离 ───────────────────────
/**
 * 把注释和字符串字面量替换成空格（保留换行），这样行号不变，
 * 而 `// 禁止出现 tauri` 这类注释不会造成误报。
 */
function stripRust(src) {
  let out = "";
  let i = 0;
  let state = "code";
  let depth = 0;
  const n = src.length;
  const blank = (k) => " ".repeat(k);

  while (i < n) {
    const ch = src[i];
    const next = src[i + 1];

    if (state === "code") {
      if (ch === "/" && next === "/") {
        state = "line";
        out += "  ";
        i += 2;
        continue;
      }
      if (ch === "/" && next === "*") {
        state = "block";
        depth = 1;
        out += "  ";
        i += 2;
        continue;
      }
      // r#"..."# / r"..." 原始字符串
      if (ch === "r" && (next === '"' || next === "#")) {
        let k = i + 1;
        let hashes = 0;
        while (src[k] === "#") {
          hashes++;
          k++;
        }
        if (src[k] === '"') {
          const closer = '"' + "#".repeat(hashes);
          const end = src.indexOf(closer, k + 1);
          const stop = end === -1 ? n : end + closer.length;
          out += src.slice(i, stop).replace(/[^\n]/g, " ");
          i = stop;
          continue;
        }
      }
      if (ch === '"') {
        state = "str";
        out += " ";
        i++;
        continue;
      }
      if (ch === "'") {
        // 字符字面量（'a' / '\n'）与生命周期（'a）要分开：后者原样保留
        if (next === "\\") {
          let j = i + 2;
          while (j < n && src[j] !== "'") j++;
          out += blank(j + 1 - i);
          i = j + 1;
          continue;
        }
        if (src[i + 2] === "'") {
          out += "   ";
          i += 3;
          continue;
        }
        out += ch;
        i++;
        continue;
      }
      out += ch;
      i++;
      continue;
    }

    if (state === "line") {
      if (ch === "\n") {
        state = "code";
        out += "\n";
        i++;
        continue;
      }
      out += " ";
      i++;
      continue;
    }

    if (state === "block") {
      if (ch === "/" && next === "*") {
        depth++;
        out += "  ";
        i += 2;
        continue;
      }
      if (ch === "*" && next === "/") {
        depth--;
        out += "  ";
        i += 2;
        if (depth === 0) state = "code";
        continue;
      }
      out += ch === "\n" ? "\n" : " ";
      i++;
      continue;
    }

    // state === "str"
    if (ch === "\\") {
      out += "  ";
      i += 2;
      continue;
    }
    if (ch === '"') {
      state = "code";
      out += " ";
      i++;
      continue;
    }
    out += ch === "\n" ? "\n" : " ";
    i++;
  }

  return out;
}

// ─────────────────────────── 收集 ───────────────────────────

function listFiles(dir, filter, acc = []) {
  for (const name of readdirSync(dir)) {
    const p = join(dir, name);
    const st = statSync(p);
    if (st.isDirectory()) {
      if (name === "target" || name === "node_modules" || name === ".git") continue;
      listFiles(p, filter, acc);
    } else if (filter(name)) {
      acc.push(p);
    }
  }
  return acc;
}

const rootTomlPath = join(ROOT, "Cargo.toml");
if (!existsSync(rootTomlPath)) {
  console.error(red(`找不到 ${rootTomlPath}，请从仓库根目录运行本脚本。`));
  process.exit(1);
}
const rootToml = parseToml(readFileSync(rootTomlPath, "utf8"));

function expandMembers(toml) {
  const ws = toml.get("workspace");
  if (!ws) throw new Error("根 Cargo.toml 没有 [workspace] 段");
  const read = (key) => {
    const e = ws.entries.get(key);
    if (!e) return [];
    return [...e.value.matchAll(/"([^"]+)"/g)].map((m) => m[1]);
  };
  const excludes = new Set(read("exclude"));
  const dirs = [];
  for (const pat of read("members")) {
    if (pat.includes("*")) {
      const base = pat.slice(0, pat.lastIndexOf("/"));
      const abs = join(ROOT, base);
      if (!existsSync(abs)) continue;
      for (const name of readdirSync(abs)) {
        const p = join(abs, name);
        if (!statSync(p).isDirectory()) continue;
        const rel = relative(ROOT, p).split("\\").join("/");
        if (excludes.has(rel)) continue;
        if (existsSync(join(p, "Cargo.toml"))) dirs.push(rel);
      }
    } else if (!excludes.has(pat)) {
      dirs.push(pat);
    }
  }
  return dirs;
}

const memberDirs = expandMembers(rootToml);
const crates = []; // { name, dir, manifest, deps: [{name,line,kind}] }

for (const dir of memberDirs) {
  const manifest = join(ROOT, dir, "Cargo.toml");
  if (!existsSync(manifest)) continue;
  const toml = parseToml(readFileSync(manifest, "utf8"));
  const pkg = toml.get("package");
  const nameEntry = pkg?.entries.get("name");
  if (!nameEntry) continue;
  const name = nameEntry.value.replace(/^["']|["']$/g, "");
  crates.push({ name, dir, manifest, toml, deps: [] });
}

const byName = new Map(crates.map((x) => [x.name, x]));

/** 把每个 crate 依赖段里指向本仓库其他 crate 的条目挑出来。 */
for (const crate of crates) {
  for (const [secName, sec] of crate.toml) {
    if (!isDepSection(secName)) continue;
    for (const [rawKey, entry] of sec.entries) {
      const key = rawKey.split(".")[0].replace(/^["']|["']$/g, "");
      if (!byName.has(key) || key === crate.name) continue;
      const kind = secName.includes("build-")
        ? "build"
        : secName.includes("dev-")
          ? "dev"
          : "normal";
      const dup = crate.deps.find((d) => d.name === key);
      if (dup) continue;
      crate.deps.push({ name: key, line: entry.line, kind, section: secName });
    }
  }
  crate.deps.sort((a, b) => a.line - b.line);
}

// ─────────────────────────── 检查 ───────────────────────────

const violations = [];
const add = (rule, file, line, message, hint) =>
  violations.push({ rule, file, line, message, hint });

// R3：所有 crate 必须在矩阵里登记
for (const crate of crates) {
  if (!(crate.name in LAYERS)) {
    add(
      "R3",
      relative(ROOT, crate.manifest).split("\\").join("/"),
      1,
      `新增 crate \`${crate.name}\` 未登记层级`,
      "先在 ARCHITECTURE.md §6 的依赖矩阵里定层，再回填本脚本的 LAYERS 表",
    );
  }
}
for (const name of Object.keys(LAYERS)) {
  if (!byName.has(name)) {
    add(
      "R3",
      "scripts/check-layers.mjs",
      0,
      `矩阵登记的 \`${name}\` 在 workspace 里不存在`,
      "删除该登记，或把它加回 Cargo.toml 的 members",
    );
  }
}

const rel = (p) => relative(ROOT, p).split("\\").join("/");

// R1：严格向下
for (const crate of crates) {
  const selfRank = LAYERS[crate.name];
  if (selfRank === undefined) continue;
  for (const dep of crate.deps) {
    const depRank = LAYERS[dep.name];
    if (depRank === undefined) continue;
    if (depRank >= selfRank) {
      add(
        "R1",
        rel(crate.manifest),
        dep.line,
        `${crate.name}（第 ${selfRank} 层）依赖了 ${dep.name}（第 ${depRank} 层）`,
        depRank === selfRank
          ? "同层之间不允许互相依赖；抽到更下层，或让上层去组合"
          : "依赖只能指向更底层；这是反向依赖，会让下层无法脱离上层单测",
      );
    }
  }
}

// R5：外壳只认 app
for (const [crateName, allowed] of Object.entries(ONLY_ALLOWED)) {
  const crate = byName.get(crateName);
  if (!crate) continue;
  for (const dep of crate.deps) {
    if (LAYERS[dep.name] === undefined) continue;
    if (allowed.has(dep.name)) continue;
    add(
      "R5",
      rel(crate.manifest),
      dep.line,
      `${crateName} 直接依赖了 ${dep.name}，绕过了 goldenagent-app`,
      "外壳只认 app 这一个入口；缺什么就在 app 里 re-export，别把外壳焊死在内部层上",
    );
  }
}

// R4：环
{
  const state = new Map(); // 0 未访问 / 1 栈上 / 2 完成
  const stack = [];
  const walk = (name) => {
    state.set(name, 1);
    stack.push(name);
    const crate = byName.get(name);
    for (const dep of crate?.deps ?? []) {
      if (!byName.has(dep.name)) continue;
      const st = state.get(dep.name) ?? 0;
      if (st === 1) {
        const at = stack.indexOf(dep.name);
        const cycle = [...stack.slice(at), dep.name];
        add(
          "R4",
          rel(crate.manifest),
          dep.line,
          `检测到依赖环：${cycle.join(" → ")}`,
          "环会让编译器直接报错；把共用的部分下沉到 core",
        );
      } else if (st === 0) {
        walk(dep.name);
      }
    }
    stack.pop();
    state.set(name, 2);
  };
  for (const crate of crates) {
    if ((state.get(crate.name) ?? 0) === 0) walk(crate.name);
  }
}

// R2：crates/** 不得出现 tauri
{
  const tauriRe = /\btauri\b/;
  for (const crate of crates) {
    if (TAURI_ALLOWED.has(crate.name)) continue;

    // 清单：剥离注释后逐行看
    const lines = readFileSync(crate.manifest, "utf8").split(/\r?\n/);
    lines.forEach((raw, idx) => {
      if (tauriRe.test(stripTomlComment(raw))) {
        add(
          "R2",
          rel(crate.manifest),
          idx + 1,
          `${crate.name} 的清单里出现了 tauri`,
          "核心 crate 一旦依赖 tauri，就再也拿不出去单独测试或移植",
        );
      }
    });
  }

  const rsFiles = listFiles(join(ROOT, "crates"), (n) => n.endsWith(".rs"));
  for (const file of rsFiles) {
    // 只检查属于"不允许 tauri"的 crate
    const owner = crates.find((c) => rel(file).startsWith(`${c.dir}/`));
    if (!owner || TAURI_ALLOWED.has(owner.name)) continue;
    const stripped = stripRust(readFileSync(file, "utf8"));
    stripped.split("\n").forEach((line, idx) => {
      if (tauriRe.test(line)) {
        add(
          "R2",
          rel(file),
          idx + 1,
          `代码里出现了 tauri`,
          "把它挪到 src-tauri，或者在 app 里包一层再暴露",
        );
      }
    });
  }
}

// ─────────────────────────── 报告 ───────────────────────────

console.log("");
console.log(bold("GoldenAgent 分层门禁"));
console.log(dim(`root: ${ROOT}`));
console.log(dim(`crate: ${crates.length}   内部依赖: ${crates.reduce((n, x) => n + x.deps.length, 0)}`));

section("依赖矩阵");
console.log(dim(` ${"层".padEnd(3)}${"crate".padEnd(22)}${"角色".padEnd(8)}内部依赖`));
for (const crate of [...crates].sort((a, b) => (LAYERS[a.name] ?? 99) - (LAYERS[b.name] ?? 99))) {
  const rank = LAYERS[crate.name];
  const deps = crate.deps.length
    ? crate.deps.map((d) => `${d.name.replace("goldenagent-", "")}(${LAYERS[d.name] ?? "?"})`).join(", ")
    : dim("—");
  console.log(
    ` ${String(rank ?? "?").padEnd(3)}${crate.name.padEnd(22)}${(TIER_NAME[rank] ?? "未登记").padEnd(8)}${deps}`,
  );
}
if (violations.length === 0) {
  section("结果");
  console.log(green("✅ R1–R5 全部通过，没有跨层依赖，没有 tauri 泄漏。"));
  console.log(dim("   规则来源：docs-local/GoldenAgent/ARCHITECTURE.md §6"));
  console.log("");
  process.exit(0);
}

section("违规清单");
const grouped = new Map();
for (const v of violations) {
  if (!grouped.has(v.rule)) grouped.set(v.rule, []);
  grouped.get(v.rule).push(v);
}
const RULE_TITLE = {
  R1: "R1 依赖必须严格向下",
  R2: "R2 crates 不得出现 tauri",
  R3: "R3 新增 crate 必须登记层级",
  R4: "R4 依赖不得成环",
  R5: "R5 外壳只认 app",
};

for (const [rule, items] of grouped) {
  console.log("");
  console.log(red(`${RULE_TITLE[rule] ?? rule} · ${items.length} 处`));
  for (const v of items) {
    const loc = v.line > 0 ? `${v.file}:${v.line}` : v.file;
    console.log(`  ${yellow(loc)}`);
    console.log(`    ${v.message}`);
    if (v.hint) console.log(dim(`    → ${v.hint}`));
  }
}

console.log("");
console.log(red(`❌ 共 ${violations.length} 处违规。修完再提交——分层是承重墙，不是建议。`));
console.log("");
process.exit(1);
