import { useState } from "react";

import type { SpaceInfo } from "../types";

interface Props {
  mode: "create" | "edit";
  space?: SpaceInfo;
  busy: boolean;
  onClose: () => void;
  onSubmit: (name: string, path: string) => Promise<void>;
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
 * 「工作目录」是这个对话框唯一有分量的字段：它决定 Agent 的 jail 根，
 * 也决定「这条会话在哪儿干活」。所以文案要把这件事说清楚，而不是写"路径"。
 */
export default function SpaceDialog({
  mode,
  space,
  busy,
  onClose,
  onSubmit,
  onDelete,
}: Props) {
  const [name, setName] = useState(space?.name ?? "");
  const [path, setPath] = useState(space?.path ?? "");
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [rootOk, setRootOk] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);

  const root = isDriveRoot(path);

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
    if (root && !rootOk) {
      setError("这是盘根：这个空间里的 Agent 能读写整块盘。确认后勾选下面的选项。");
      return;
    }

    setSaving(true);
    setError(null);
    try {
      await onSubmit(cleanName, cleanPath);
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
                <p>
                  {space.sessions} 条会话会移回「任务」，目录里的文件不受影响。
                </p>
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
