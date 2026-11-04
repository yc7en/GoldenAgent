interface Props {
  value: string;
  busy: boolean;
  modeLabel: string;
  onChange: (value: string) => void;
  onSend: () => void;
  onCancel: () => void;
}

export default function Composer({ value, busy, modeLabel, onChange, onSend, onCancel }: Props) {
  return (
    <footer className="composer">
      <textarea
        value={value}
        placeholder={`给 GoldenAgent 说点什么（当前权限：${modeLabel}）`}
        rows={3}
        onChange={(event) => onChange(event.target.value)}
        onKeyDown={(event) => {
          if (event.key === "Enter" && !event.shiftKey) {
            event.preventDefault();
            onSend();
          }
        }}
      />
      <div className="composer-actions">
        <span className="hint">
          {busy ? "运行中…（停止后在轮次边界生效）" : "Enter 发送 · Shift+Enter 换行"}
        </span>
        {busy ? (
          <button type="button" className="ghost" onClick={onCancel}>
            停止
          </button>
        ) : (
          <button
            type="button"
            className="primary"
            onClick={onSend}
            disabled={value.trim().length === 0}
          >
            发送
          </button>
        )}
      </div>
    </footer>
  );
}
