interface Props {
  title: string;
  workspace: string;
  /** 附加目录数量。0 就不显示——没有的时候不该多一个「+0」。 */
  extraRoots: number;
  spaceName: string;
  searchOpen: boolean;
  onToggleSearch: () => void;
}

/** 对话头：一眼看清「在哪个空间、哪条会话、哪个目录」。 */
export default function ChatHead({
  title,
  workspace,
  extraRoots,
  spaceName,
  searchOpen,
  onToggleSearch,
}: Props) {
  return (
    <header className="chat-head">
      <div className="chat-head-text">
        <div className="crumb">
          <span>{spaceName}</span>
          <span className="crumb-sep">/</span>
          <span className="crumb-current">{title}</span>
        </div>
        <p
          className="chat-meta"
          title={
            extraRoots > 0
              ? `${workspace}（另附加 ${extraRoots} 个目录）`
              : workspace
          }
        >
          {workspace}
          {extraRoots > 0 && <span className="chat-meta-extra">+{extraRoots}</span>}
        </p>
      </div>

      <button
        type="button"
        className={searchOpen ? "icon-btn on" : "icon-btn"}
        onClick={onToggleSearch}
        title="搜索当前会话（含本次思考）"
        aria-label="搜索当前会话"
      >
        <svg width="15" height="15" viewBox="0 0 16 16" aria-hidden="true">
          <circle cx="7" cy="7" r="4.5" fill="none" stroke="currentColor" />
          <line x1="10.5" y1="10.5" x2="14" y2="14" stroke="currentColor" />
        </svg>
      </button>
    </header>
  );
}
