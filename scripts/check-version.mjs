// 版本号一致性检查（零依赖，`npm run check:version`）。
//
// 版本号在这套工程里出现在三个地方，而且没有任何构建工具会替你同步：
//
//   package.json          → 前端产物 / npm 包
//   src-tauri/tauri.conf.json → 安装包、可执行文件属性、关于面板
//   Cargo.toml [workspace.package] → 所有 crate 的 version
//
// 三处不一致的后果不是报错，而是"安装包显示 0.1.0、程序里显示 0.2.0"
// 这种只在出问题后才被发现的偏差。所以每次 verify 都校一遍。

import fs from "node:fs";
import path from "node:path";

const root = process.cwd();
const problems = [];

function read(file) {
  const full = path.join(root, file);
  if (!fs.existsSync(full)) {
    problems.push(`找不到 ${file}`);
    return null;
  }
  return fs.readFileSync(full, "utf8");
}

// --- package.json ---
const pkgText = read("package.json");
const pkgVersion = pkgText ? JSON.parse(pkgText).version : null;

// --- tauri.conf.json ---
const tauriText = read(path.join("src-tauri", "tauri.conf.json"));
const tauriVersion = tauriText ? JSON.parse(tauriText).version : null;

// --- Cargo.toml 的 [workspace.package] version ---
const cargoText = read("Cargo.toml");
let cargoVersion = null;
if (cargoText) {
  const section = cargoText.split(/^\[/m).find((block) => block.startsWith("workspace.package]"));
  const match = section?.match(/^\s*version\s*=\s*"([^"]+)"/m);
  cargoVersion = match ? match[1] : null;
  if (!cargoVersion) {
    problems.push("Cargo.toml 的 [workspace.package] 里没找到 version");
  }
}

// --- 各 crate 必须继承 workspace 版本，不许自己写死 ---
for (const dir of ["src-tauri", ...fs.readdirSync(path.join(root, "crates")).map((name) => path.join("crates", name))]) {
  const file = path.join(dir, "Cargo.toml");
  const text = read(file);
  if (!text) continue;
  const own = /^\[package\][\s\S]*?^version\s*=\s*"([^"]+)"/m.exec(text);
  if (own) {
    problems.push(`${file} 写死了 version = "${own[1]}"，应改成 version.workspace = true`);
  }
  if (!/version\.workspace\s*=\s*true/.test(text)) {
    problems.push(`${file} 缺少 version.workspace = true`);
  }
}

const versions = [
  ["package.json", pkgVersion],
  ["src-tauri/tauri.conf.json", tauriVersion],
  ["Cargo.toml", cargoVersion],
];

for (const [file, version] of versions) {
  if (!version) problems.push(`${file} 读不到版本号`);
}

const distinct = [...new Set(versions.map(([, v]) => v).filter(Boolean))];
if (distinct.length > 1) {
  problems.push(
    `三处版本号不一致：${versions.map(([f, v]) => `${f}=${v}`).join(" / ")}`,
  );
}

if (problems.length > 0) {
  console.error("✗ 版本检查未通过：");
  for (const problem of problems) console.error(`  - ${problem}`);
  process.exit(1);
}

console.log(`✓ 版本一致：${distinct[0]}（package.json / tauri.conf.json / Cargo.toml）`);
