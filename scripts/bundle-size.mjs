// 安装包体积检查（零依赖，`npm run bundle:size`）。
//
// 为什么单独一个脚本而不是塞进 verify：verify 不跑 release 构建
// （那是几分钟的事，每次改一行前端都跑一遍不现实）。
// 但"发版前有没有突然胖了一倍"必须有人看，所以留一条随时可跑的命令。
//
// 预算不是拍脑袋：Tauri 2 空壳约 3–5 MiB，加上 reqwest + native-tls
// 与 React 产物，主程序到 12 MiB 属正常。超过 20 MiB 基本就是
// 误把 debug 信息或第二份 TLS 实现打进去了。

import fs from "node:fs";
import path from "node:path";

const MiB = 1024 * 1024;
const BUDGET = {
  binary: 20 * MiB,
  installer: 10 * MiB,
};

const root = process.cwd();

// Cargo workspace 的 target 在**仓库根**，不在 src-tauri 下——
// 单 crate 工程才会是 src-tauri/target。两个位置都找一次，报错少一次解释。
const candidates = [
  path.join(root, "target", "release"),
  path.join(root, "src-tauri", "target", "release"),
];
const releaseDir = candidates.find((dir) => fs.existsSync(dir));

if (!releaseDir) {
  console.error("✗ 还没有 release 产物。先跑：npm run tauri build");
  console.error(`  找过：${candidates.map((dir) => path.relative(root, dir)).join(" / ")}`);
  process.exit(1);
}

const bundleDir = path.join(releaseDir, "bundle");

/** 递归找文件，跳过目录本身（.app 整体算体积时单独处理）。 */
function walk(dir, out = []) {
  if (!fs.existsSync(dir)) return out;
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      walk(full, out);
    } else if (entry.isFile()) {
      out.push({ path: full, size: fs.statSync(full).size });
    }
  }
  return out;
}

/** 目录总大小（macOS 的 .app 是个目录，不能只看里面的文件个数）。 */
function dirSize(dir) {
  return walk(dir).reduce((sum, file) => sum + file.size, 0);
}

const binaryNames = ["goldenagent-shell.exe", "goldenagent-shell", "GoldenAgent"];
const binaries = binaryNames
  .map((name) => path.join(releaseDir, name))
  .filter((file) => fs.existsSync(file))
  .map((file) => ({ path: file, size: fs.statSync(file).size, kind: "binary" }));

const installerExts = [".exe", ".msi", ".dmg", ".deb", ".AppImage", ".rpm"];
const installers = walk(bundleDir).filter((file) =>
  installerExts.some((ext) => file.path.endsWith(ext)),
);

const appBundles = [];
for (const ext of [".app"]) {
  const found = [];
  const scan = (dir) => {
    if (!fs.existsSync(dir)) return;
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      const full = path.join(dir, entry.name);
      if (entry.isDirectory()) {
        if (entry.name.endsWith(ext)) {
          found.push({ path: full, size: dirSize(full), kind: "bundle" });
        } else {
          scan(full);
        }
      }
    }
  };
  scan(bundleDir);
  appBundles.push(...found);
}

const rows = [
  ...binaries.map((file) => ({ ...file, kind: "binary" })),
  ...installers.map((file) => ({ ...file, kind: "installer" })),
  ...appBundles,
];

if (rows.length === 0) {
  console.error("✗ 没找到可执行文件或安装包。先跑：npm run tauri build");
  process.exit(1);
}

const mib = (bytes) => `${(bytes / MiB).toFixed(2)} MiB`;
const relative = (file) => path.relative(process.cwd(), file);

let failed = false;
console.log("产物体积：");
for (const row of rows) {
  const budget = BUDGET[row.kind];
  const verdict =
    budget === undefined
      ? ""
      : row.size > budget
        ? `  ✗ 超出预算 ${mib(budget)}`
        : `  ✓ 预算 ${mib(budget)}`;
  if (budget !== undefined && row.size > budget) failed = true;
  console.log(`  ${mib(row.size).padStart(9)}  ${relative(row.path)}${verdict}`);
}

// 顺便对着"是不是打了两份 TLS"这条常见病做个提示。
const binary = rows.find((row) => row.kind === "binary");
if (binary && binary.size > 25 * MiB) {
  console.log(
    "\n提示：主程序超过 25 MiB，通常是把 debug 信息或第二份 TLS/HTTP 栈打进去了。" +
      '\n检查 Cargo.toml 里 reqwest 的 default-features 是否仍为 false。',
  );
}

if (failed) {
  console.error("\n✗ 有产物超出体积预算");
  process.exit(1);
}

console.log("\n✓ 体积在预算内");
