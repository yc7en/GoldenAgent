/** 侧栏里的时间只需要"多久以前"，精确到分钟就够了。 */
export function relativeTime(ms: number): string {
  const diff = Date.now() - ms;
  if (diff < 0) return "刚刚";
  if (diff < 60_000) return "刚刚";
  if (diff < 3_600_000) return `${Math.floor(diff / 60_000)} 分钟前`;
  if (diff < 86_400_000) return `${Math.floor(diff / 3_600_000)} 小时前`;
  const date = new Date(ms);
  return `${date.getMonth() + 1}月${date.getDate()}日`;
}

/** 目录太长时只留最后两段，菜单里放不下整条路径。 */
export function shortPath(path: string): string {
  const parts = path.split(/[\\/]+/).filter(Boolean);
  if (parts.length <= 2) return path;
  return `…${path.includes("\\") ? "\\" : "/"}${parts.slice(-2).join(path.includes("\\") ? "\\" : "/")}`;
}
