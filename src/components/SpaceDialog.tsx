import { useState } from "react";

import type { SpaceInfo } from "../types";

interface Props {
  mode: "create" | "edit";
  space?: SpaceInfo;
  /** 一个空间最多能附加几个目录。由后端下发，前端不写死。 */
  maxExtra: number;
  busy: boolean;
  onClose: () => void;
  onSubmit: (name: string, path: string, extraPaths: string[]) => Promise<void>;
  onDelete?: (space: SpaceInfo) => Promise<void>;
}

/** `C:\`、`C:/`、`/` 这类：整块盘都交给 Agent，值得再问一次。 */
function isDriveRoot(path: string): boolean {
  const trimmed = path.trim();
  return /^[A-Za-z]:[\\/]?$/.test(trimmed) || trimmed === "/";
}

/**
 * 新建 / 编辑空间。
 *
 * 「工作目录」是这个对话框最有分量的字段：它决定 Agent 的 jail 根，
 * 也决定「这条会话在哪儿干活」。附加目录是同一件事的延伸——
 * 它们也被 jail 覆盖，但**不参与相对路径解析**，这一点必须在界面上说清，
 * 否则模型说"读 a.txt"落到哪个目录这件事，用户会以为是随机的。
 */
export default function SpaceDialog({
  mode,
  space,
  maxExtra,
  busy,
  onClose,
  onSubmit,
  onDelete,
}: Props) {
  const [name, setName] = useState(space?.name ?? "");
  const [path, setPath] = useState(space?.path ?? "");
  const [extraPaths, setExtraPaths] = useState<string[]>(space?.extra_paths ?? []);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [rootOk, setRootOk] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);

  // 主目录或任一个附加目录是盘根，都要同一句确认。
  const root = isDriveRoot(path) || extraPaths.some(isDriveRoot);
  const full = extraPaths.length >= maxExtra;

  function updateExtra(index: number, value: string): void {
    setExtraPaths((prev) => prev.map((item, i) => (i === index ? value : item)));
    setRootOk(false);
  }

  function removeExtra(index: number): void {
    setExtraPaths((prev) => prev.filter((_, i) => i !== index));
  }

  async function submit(): Promise<void> {
    if (saving) return;
    const cleanName = name.trim();
    const cleanPath = path.trim();
    if (!cleanName) {
      setError("空间名不能为空");
      return;
    }
    if (!cleanPath) {
      setError("工作目录不能为空");
      return;
    }
    // 空行是"还没填"，不是"要附加一个空目录"。后端也会过滤，但在这里拦住
    // 能给出更准确的说法——否则用户得到的是"目录不能为空"这种不像自己造的错。
    const cleanExtra = extraPaths.map((item) => item.trim()).filter((item) => item.length > 0);
    if (cleanExtra.length > maxExtra) {
      setError(`附加目录最多 ${maxExtra} 个`);
      return;
    }
    if (root && !rootOk) {
      setError("有目录是盘根：这个空间里的 Agent 能读写整块盘。确认后勾选下面的选项。");
      return;
    }

    setSaving(true);
    setError(null);
    try {
      await onSubmit(cleanName, cleanPath, cleanExtra);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setSaving(false);
    }
  }

  async function remove(): Promise<void> {
    if (!space || !onDelete || saving) return;
    setSaving(true);
    setError(null);
    try {
      await onDelete(space);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setSaving(false);
    }
  }

  return (
    <div
      className="overlay"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget) onClose();
      }}
    >
      <div className="dlg" role="dialog" aria-label={mode === "create" ? "新建空间" : "编辑空间"}>
        <header className="dlg-head">
          <h2>{mode === "create" ? "新建空间" : "编辑空间"}</h2>
          <button type="button" className="icon-btn" onClick={onClose} aria-label="关闭">
            ✕
          </button>
        </header>

        <div className="dlg-body">
          <p className="dlg-lead">
            空间是一块可以切换的工作目录。这个空间里的会话，工具读写都限制在它下面。
          </p>

          <label className="field">
            <span>空间名</span>
            <input
              value={name}
              autoFocus
              placeholder="例如 GoldenRules"
              onChange={(event) => setName(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter") {
                  event.preventDefault();
                  void submit();
                }
              }}
            />
          </label>

          <label className="field">
            <span>工作目录</span>
            <input
              value={path}
              placeholder="绝对路径，例如 J:\CODE\GoldenRules"
              onChange={(event) => {
                setPath(event.target.value);
                setRootOk(false);
              }}
              onKeyDown={(event) => {
                if (event.key === "Enter") {
                  event.preventDefault();
                  void submit();
                }
              }}
            />
          </label>

          <p className="dlg-hint">
            目录不存在会顺手建出来；已存在的目录不会被改动。删除空间只删应用自己的会话记录，
            不碰这个目录里的任何文件。
          </p>

          <div className="dlg-extra">
            <div className="dlg-extra-head">
              <strong>附加目录</strong>
              <span className="dlg-extra-count">
                {extraPaths.length} / {maxExtra}
              </span>
            </div>
            <p className="dlg-hint">
              这些目录同样可以读写，适合「代码在一个仓库、图纸在另一个盘」这种情形。
              注意：只有绝对路径会落到附加目录里，相对路径一律以工作目录为准；
              文件搜索默认也只扫工作目录，别把一个整盘挂进来。
            </p>

            {extraPaths.map((item, index) => (
              <div className="dlg-extra-row" key={index}>
                <input
                  value={item}
                  placeholder="绝对路径，例如 D:\specs"
                  onChange={(event) => updateExtra(index, event.target.value)}
                  onKeyDown={(event) => {
                    if (event.key === "Enter") {
                      event.preventDefault();
                      void submit();
                    }
                  }}
                />
                <button
                  type="button"
                  className="icon-btn"
                  onClick={() => removeExtra(index)}
                  aria-label={`移除第 ${index + 1} 个附加目录`}
                  title="移除"
                >
                  ✕
                </button>
              </div>
            ))}

            <button
              type="button"
              className="ghost dlg-extra-add"
              onClick={() => setExtraPaths((prev) => [...prev, ""])}
              disabled={full}
              title={full ? `最多 ${maxExtra} 个` : undefined}
            >
              ＋ 添加目录
            </button>
          </div>

          {root && (
            <label className="dlg-check">
              <input
                type="checkbox"
                checked={rootOk}
                onChange={(event) => setRootOk(event.target.checked)}
              />
              <span>我知道：这个空间里的 Agent 能读写整块盘</span>
            </label>
          )}

          {error && <p className="dlg-err">{error}</p>}

          {mode === "edit" && space && (
            <div className="dlg-danger">
              <div>
                <strong>删除空间</strong>
                <p>{space.sessions} 条会话会移回「任务」，目录里的文件不受影响。</p>
              </div>
              {confirmDelete ? (
                <div className="dlg-danger-actions">
                  <button type="button" className="ghost" onClick={() => setConfirmDelete(false)}>
                    取消
                  </button>
                  <button type="button" className="danger" onClick={() => void remove()} disabled={busy}>
                    确定删除
                  </button>
                </div>
              ) : (
                <button
                  type="button"
                  className="ghost"
                  onClick={() => setConfirmDelete(true)}
                  disabled={busy}
                  title={busy ? "运行中不能删空间" : undefined}
                >
                  删除…
                </button>
              )}
            </div>
          )}
        </div>

        <footer className="dlg-foot">
          <button type="button" className="ghost" onClick={onClose}>
            取消
          </button>
          <button type="button" className="primary" onClick={() => void submit()} disabled={saving}>
            {saving ? "保存中…" : mode === "create" ? "创建空间" : "保存"}
          </button>
        </footer>
      </div>
    </div>
  );
}
