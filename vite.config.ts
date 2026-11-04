import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri 期望一个固定端口的开发服务器，端口被占就直接失败，
// 不要让 vite 悄悄换一个——那会让 Tauri 白屏。
//
// 端口选 5320 而不是 Vite 默认的 5173 或 Tauri 文档常见的 1420：
// 1420/1421 在本机落在 Windows 的 TCP 保留端口区间内，listen 直接 EACCES；
// 5173 常年被别的项目占着。5320 已验证可绑定。
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    host: "127.0.0.1",
    port: 5320,
    strictPort: true,
    watch: {
      ignored: ["**/src-tauri/**", "**/target/**"],
    },
  },
  build: {
    // Tauri 的 WebView 在 Windows 上是 WebView2（Chromium），
    // 不需要兼容老浏览器，压缩产物即可。
    target: "chrome105",
    outDir: "dist",
    emptyOutDir: true,
  },
});
