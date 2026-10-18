import { KeyboardEvent, useRef, useState } from "react";

interface Props {
  dispatching: boolean;
  statusLine: string;
  statusIsError: boolean;
  onSend: (question: string) => void;
}

export default function InputBar({
  dispatching,
  statusLine,
  statusIsError,
  onSend,
}: Props) {
  const [text, setText] = useState("");
  const taRef = useRef<HTMLTextAreaElement>(null);

  const canSend = !dispatching && text.trim().length > 0;

  const send = () => {
    if (!canSend) return;
    onSend(text.trim());
    setText("");
    taRef.current?.focus();
  };

  const onKeydown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
      e.preventDefault();
      send();
    }
  };

  return (
    <div className="inputbar">
      {/* 状态行放输入框上方、仅有内容时渲染：
          空状态不占位（否则固定吃掉 ~20px，输入框整体悬在半空），
          出现/消失时输入行距窗口底边的距离保持不变 */}
      {statusLine && (
        <div className={`status-line${statusIsError ? " err" : ""}`}>
          {statusLine}
        </div>
      )}

      <div className="input-row">
        <textarea
          ref={taRef}
          value={text}
          placeholder="问点什么？将同时发往全部站点（Enter 发送，Shift+Enter 换行）"
          onChange={(e) => setText(e.target.value)}
          onKeyDown={onKeydown}
          rows={1}
        />
        <button className="send-btn" disabled={!canSend} onClick={send}>
          {dispatching ? "发送中…" : "发送"}
        </button>
      </div>
    </div>
  );
}
