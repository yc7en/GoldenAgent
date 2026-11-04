import { useState } from "react";

import { api } from "../api";
import type { AppInfo, ProviderSetup, Snapshot } from "../types";
import { PROVIDER_LABELS } from "../types";

const WORKSPACE_LABELS: Record<string, string> = {
  env: "环境变量指定",
  cwd: "当前目录",
  home: "家目录默认",
};

interface Props {
  setup: ProviderSetup | null;
  info: AppInfo | null;
  snapshot: Snapshot | null;
  onClose: () => void;
  onSaved: (setup: ProviderSetup) => void;
  onGoSpaces: () => void;
}

export default function SettingsSheet({
  setup,
  info,
  snapshot,
  onClose,
  onSaved,
  onGoSpaces,
}: Props) {
  const [form, setForm] = useState({
    kind: setup?.kind ?? "anthropic",
    model: setup?.model ?? "",
    api_key: "",
    base_url: setup?.base_url ?? "",
    // 用字符串存：输入框清空时不能立刻变成 0，那会被后端夹成最小值。
    max_tokens: setup ? String(setup.max_tokens) : "",
  });
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);

  const providerState = snapshot?.provider_state ?? "unconfigured";
  const currentOption = setup?.catalog.find((option) => option.id === form.kind);
  const keyPlaceholder =
    setup?.kind === form.kind && providerState === "ready"
      ? "留空表示不修改已保存的 Key"
      : currentOption?.needs_key === false
        ? "这一项不需要 Key"
        : "必填";

  async function save(): Promise<void> {
    if (saving) return;
    setSaving(true);
    setSaveError(null);

    try {
      const next = await api.saveProvider({
        kind: form.kind,
        model: form.model.trim(),
        // 空串表达"没改 key"。后端据此保留原来那份——
        // 界面从来拿不到旧 key，所以不能用"回填再提交"的办法。
        api_key: form.api_key.length > 0 ? form.api_key : null,
        base_url: form.base_url.trim(),
        // 空框 = 不改。填了非数字也当"不改"，别把一个 NaN 发给后端。
        max_tokens:
          form.max_tokens.trim().length > 0 && Number.isFinite(Number(form.max_tokens))
            ? Number(form.max_tokens)
            : null,
      });
      onSaved(next);
    } catch (error) {
      setSaveError(error instanceof Error ? error.message : String(error));
    } finally {
      setSaving(false);
    }
  }

  async function copyTemplate(): Promise<void> {
    if (!setup) return;
    try {
      await navigator.clipboard.writeText(setup.template);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1500);
    } catch {
      // 剪贴板被拒时不要静默：用户会以为复制成功了。
      setSaveError("复制失败，请展开下面的模板手动选择");
    }
  }

  return (
    <div
      className="sheet-backdrop"
      onMouseDown={(event) => {
        // 只在点空白处关闭；拖选文字时松手落在遮罩上不该误关。
        if (event.target === event.currentTarget) onClose();
      }}
    >
      <div className="sheet" role="dialog" aria-label="设置">
        <header className="sheet-head">
          <h2>设置</h2>
          <button type="button" className="icon" onClick={onClose}>
            关闭
          </button>
        </header>

        <div className="sheet-body">
          <section>
            <h3>模型</h3>
            <p className="sheet-note">
              保存后立即生效，不用重启。配置写在下面的文件里，只存在本机。
            </p>

            <label className="field">
              <span>厂商</span>
              <select
                value={form.kind}
                onChange={(event) => {
                  const kind = event.target.value;
                  const option = setup?.catalog.find((item) => item.id === kind);
                  setForm((prev) => ({
                    ...prev,
                    kind,
                    // 换厂商就把模型名重置成该厂商的默认值；
                    // 留着上一家的模型名只会换来一个 404。
                    model: option?.default_model ?? "",
                  }));
                }}
              >
                {(setup?.catalog ?? [{ id: "mock", default_model: "mock", needs_key: false }]).map(
                  (option) => (
                    <option key={option.id} value={option.id}>
                      {PROVIDER_LABELS[option.id] ?? option.id}
                    </option>
                  ),
                )}
              </select>
            </label>

            <label className="field">
              <span>模型名</span>
              <input
                value={form.model}
                placeholder={currentOption?.default_model ?? "留空使用厂商默认"}
                onChange={(event) => setForm((prev) => ({ ...prev, model: event.target.value }))}
              />
            </label>

            <label className="field">
              <span>API Key</span>
              <input
                type="password"
                value={form.api_key}
                placeholder={keyPlaceholder}
                autoComplete="off"
                disabled={currentOption?.needs_key === false}
                onChange={(event) => setForm((prev) => ({ ...prev, api_key: event.target.value }))}
              />
            </label>

            <label className="field">
              <span>接口地址</span>
              <input
                value={form.base_url}
                placeholder="留空使用官方地址；自建网关/代理时填这里"
                onChange={(event) => setForm((prev) => ({ ...prev, base_url: event.target.value }))}
              />
            </label>

            <label className="field">
              <span>最大输出</span>
              <input
                type="number"
                min={256}
                max={32768}
                step={256}
                value={form.max_tokens}
                placeholder={String(setup?.max_tokens ?? 8192)}
                onChange={(event) =>
                  setForm((prev) => ({ ...prev, max_tokens: event.target.value }))
                }
              />
            </label>
            <p className="sheet-note">
              推理模型的思考过程也占用这份额度。额度被思考吃光时，正文一个字都不会出现——
              碰到"全用在思考上"的报错，先把它调大。
            </p>

            {saveError && <p className="sheet-error">{saveError}</p>}

            <div className="sheet-actions">
              <button type="button" className="primary" onClick={() => void save()} disabled={saving}>
                {saving ? "保存中…" : "保存并生效"}
              </button>
            </div>
          </section>

          <section>
            <h3>空间</h3>
            <p className="sheet-note">
              空间是一块可以切换的工作目录。会话跟着空间走，换目录不搬历史。
            </p>
            {snapshot?.spaces_warning ? (
              <p className="sheet-error">{snapshot.spaces_warning}</p>
            ) : null}
            <div className="sheet-actions">
              <button type="button" className="ghost" onClick={onGoSpaces}>
                管理空间
              </button>
            </div>
          </section>

          <section>
            <h3>配置文件</h3>
            <code className="sheet-path">{setup?.config_path ?? "—"}</code>
            <p className="sheet-note">
              手动改这个文件也可以，改完重启生效。里面的 Key 是明文，别把它提交到仓库。
            </p>
            <div className="sheet-actions">
              <button type="button" className="ghost" onClick={() => void copyTemplate()}>
                复制模板
              </button>
              {copied && <span className="sheet-ok">已复制</span>}
            </div>
            <details>
              <summary>看模板</summary>
              <pre className="sheet-pre">{setup?.template ?? ""}</pre>
            </details>
          </section>

          <section>
            <h3>关于</h3>
            <dl className="sheet-facts">
              <dt>版本</dt>
              <dd>{info?.version ?? snapshot?.version ?? "—"}</dd>
              <dt>标识</dt>
              <dd>{info?.identifier ?? "—"}</dd>
              <dt>工作区</dt>
              <dd>
                {snapshot?.workspace ?? info?.workspace ?? "—"}
                {info && (
                  <span className="sheet-tag">
                    {WORKSPACE_LABELS[info.workspace_origin] ?? info.workspace_origin}
                  </span>
                )}
              </dd>
            </dl>
            {info?.update_url && (
              <a className="sheet-link" href={info.update_url} target="_blank" rel="noreferrer">
                检查更新
              </a>
            )}
          </section>
        </div>
      </div>
    </div>
  );
}
