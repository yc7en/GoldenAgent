import { Fragment } from "react";

interface Props {
  text: string;
  query: string;
}

/** 把命中的词包成 `<mark>`。大小写不敏感，但保留原文大小写。 */
export default function Highlight({ text, query }: Props) {
  const needle = query.trim().toLowerCase();
  if (!needle) return <>{text}</>;

  const haystack = text.toLowerCase();
  const parts: React.ReactNode[] = [];
  let cursor = 0;

  for (;;) {
    const at = haystack.indexOf(needle, cursor);
    if (at < 0) break;
    if (at > cursor) parts.push(<Fragment key={`t${cursor}`}>{text.slice(cursor, at)}</Fragment>);
    parts.push(<mark key={`m${at}`}>{text.slice(at, at + needle.length)}</mark>);
    cursor = at + needle.length;
  }

  if (cursor === 0) return <>{text}</>;
  if (cursor < text.length) parts.push(<Fragment key="tail">{text.slice(cursor)}</Fragment>);
  return <>{parts}</>;
}
