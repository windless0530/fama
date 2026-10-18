import { KeyboardEvent, useRef, useState } from "react";

interface Props {
  fillOnly: boolean;
  onToggleFillOnly: () => void;
  dispatching: boolean;
  statusLine: string;
  statusIsError: boolean;
  onSend: (question: string) => void;
}

export default function InputBar({
  fillOnly,
  onToggleFillOnly,
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
      <div className="input-row">
        <textarea
          ref={taRef}
          value={text}
          placeholder="问点什么？将同时发往全部站点（Enter 发送，Shift+Enter 换行）"
          onChange={(e) => setText(e.target.value)}
          onKeyDown={onKeydown}
          rows={1}
        />
        <label className="toggle-fill" onClick={onToggleFillOnly}>
          <input
            type="checkbox"
            checked={fillOnly}
            onChange={onToggleFillOnly}
          />
          仅填充
        </label>
        <button className="send-btn" disabled={!canSend} onClick={send}>
          {dispatching ? "发送中…" : "发送"}
        </button>
      </div>

      <div className={`status-line${statusIsError ? " err" : ""}`}>
        {statusLine}
      </div>
    </div>
  );
}
