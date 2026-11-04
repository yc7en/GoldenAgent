// 生成应用图标（纯 Node，不引第三方依赖）。
//
// 为什么手写 PNG 编码器：图标是构建产物，不该往仓库里塞二进制 blob，
// 也不该为了四张图装一个 sharp。跑一次就能重新生成：
//   node scripts/gen-icon.mjs

import zlib from "node:zlib";
import fs from "node:fs";
import path from "node:path";

const CRC_TABLE = (() => {
  const table = new Int32Array(256);
  for (let n = 0; n < 256; n += 1) {
    let c = n;
    for (let k = 0; k < 8; k += 1) {
      c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    }
    table[n] = c;
  }
  return table;
})();

function crc32(buffer) {
  let crc = 0xffffffff;
  for (const byte of buffer) {
    crc = CRC_TABLE[(crc ^ byte) & 0xff] ^ (crc >>> 8);
  }
  return (crc ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
  const length = Buffer.alloc(4);
  length.writeUInt32BE(data.length, 0);
  const body = Buffer.concat([Buffer.from(type, "ascii"), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body), 0);
  return Buffer.concat([length, body, crc]);
}

/** 深底 + 金色斜纹方块，居中留一个 G 形的缺口块。 */
function render(size) {
  const raw = Buffer.alloc((size * 4 + 1) * size);
  const radius = Math.max(2, Math.round(size * 0.18));
  const inset = Math.round(size * 0.22);

  for (let y = 0; y < size; y += 1) {
    const rowStart = y * (size * 4 + 1);
    raw[rowStart] = 0; // filter: none

    for (let x = 0; x < size; x += 1) {
      const offset = rowStart + 1 + x * 4;

      // 圆角矩形遮罩
      const dx = Math.min(x, size - 1 - x);
      const dy = Math.min(y, size - 1 - y);
      let inside = true;
      if (dx < radius && dy < radius) {
        const cxx = radius - dx;
        const cyy = radius - dy;
        inside = cxx * cxx + cyy * cyy <= radius * radius;
      }

      if (!inside) {
        raw[offset] = 0;
        raw[offset + 1] = 0;
        raw[offset + 2] = 0;
        raw[offset + 3] = 0;
        continue;
      }

      // 背景：深蓝黑到近黑的对角渐变
      const t = (x + y) / (2 * size);
      let r = Math.round(18 + 16 * t);
      let g = Math.round(22 + 20 * t);
      let b = Math.round(30 + 34 * t);

      // 中间的 G 形：方块右侧上半挖掉一条，下半留出"下巴"
      const inBox =
        x >= inset && x < size - inset && y >= inset && y < size - inset;
      const notchX = x >= size * 0.5 && x < size - inset;
      const notchY = y >= size * 0.3 && y < size * 0.5;

      if (inBox && !(notchX && notchY)) {
        // 金色渐变
        const k = (x - inset) / (size - inset * 2);
        r = Math.round(245 - 30 * k);
        g = Math.round(190 - 40 * k);
        b = Math.round(40 + 10 * k);
      }

      raw[offset] = r;
      raw[offset + 1] = g;
      raw[offset + 2] = b;
      raw[offset + 3] = 255;
    }
  }

  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(size, 0);
  ihdr.writeUInt32BE(size, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // RGBA
  ihdr[10] = 0;
  ihdr[11] = 0;
  ihdr[12] = 0;

  const idat = zlib.deflateSync(raw, { level: 9 });

  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", ihdr),
    chunk("IDAT", idat),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

/** ICO 容器：Vista 之后可以直接内嵌 PNG。 */
function ico(pngs) {
  const header = Buffer.alloc(6);
  header.writeUInt16LE(0, 0);
  header.writeUInt16LE(1, 2);
  header.writeUInt16LE(pngs.length, 4);

  const entries = [];
  let offset = 6 + pngs.length * 16;
  for (const { size, data } of pngs) {
    const entry = Buffer.alloc(16);
    entry[0] = size >= 256 ? 0 : size;
    entry[1] = size >= 256 ? 0 : size;
    entry[2] = 0;
    entry[3] = 0;
    entry.writeUInt16LE(1, 4);
    entry.writeUInt16LE(32, 6);
    entry.writeUInt32LE(data.length, 8);
    entry.writeUInt32LE(offset, 12);
    offset += data.length;
    entries.push(entry);
  }

  return Buffer.concat([header, ...entries, ...pngs.map((p) => p.data)]);
}

/**
 * ICNS 容器：'icns' + 总长度 + 若干 [类型 + 长度 + 数据]。
 *
 * macOS 10.7 起每个块可以直接放 PNG，所以不必生成老的 RGB24 编码。
 * 类型是固定表：`ic07`=128、`ic08`=256、`ic09`=512、`ic10`=1024。
 */
function icns(pngs) {
  const TYPE_BY_SIZE = { 128: "ic07", 256: "ic08", 512: "ic09", 1024: "ic10" };

  const entries = pngs.map(({ size, data }) => {
    const type = TYPE_BY_SIZE[size];
    if (!type) throw new Error(`ICNS 没有 ${size}px 的槽位`);
    const head = Buffer.alloc(8);
    head.write(type, 0, "ascii");
    head.writeUInt32BE(data.length + 8, 4);
    return Buffer.concat([head, data]);
  });

  const total = 8 + entries.reduce((sum, entry) => sum + entry.length, 0);
  const header = Buffer.alloc(8);
  header.write("icns", 0, "ascii");
  header.writeUInt32BE(total, 4);
  return Buffer.concat([header, ...entries]);
}

const outDir = path.join(process.cwd(), "src-tauri", "icons");
fs.mkdirSync(outDir, { recursive: true });

// 一次渲染到最大尺寸，其余尺寸各自渲染——缩放在纯 JS 里做反而更糊。
const pngFor = (size) => ({ size, data: render(size) });

const icon32 = pngFor(32);
const icon128 = pngFor(128);
const icon256 = pngFor(256);
const icon512 = pngFor(512);
const icon1024 = pngFor(1024);

fs.writeFileSync(path.join(outDir, "32x32.png"), icon32.data);
fs.writeFileSync(path.join(outDir, "128x128.png"), icon128.data);
fs.writeFileSync(path.join(outDir, "128x128@2x.png"), icon256.data);
// Linux 打包认这一张；也是给"手动换图标"的人一个方便取用的原图。
fs.writeFileSync(path.join(outDir, "icon.png"), icon512.data);

// Windows：16/32/48 是任务栏与资源管理器真正用到的三档，256 是缩略图。
fs.writeFileSync(
  path.join(outDir, "icon.ico"),
  ico([pngFor(16), icon32, pngFor(48), icon256]),
);

// macOS：128/256/512/1024 四档，Retina 才不缺图。
fs.writeFileSync(
  path.join(outDir, "icon.icns"),
  icns([icon128, icon256, icon512, icon1024]),
);

console.log(
  `icon 已生成: png 32/128/256/512, ico 16/32/48/256, icns 128/256/512/1024 -> ${outDir}`,
);
