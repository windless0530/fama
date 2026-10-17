// Fama 通用注入脚本（design.md §2.4）
// @version 1
//
// 契约：必须实现 famaFill(text) / famaSend()，返回 { ok, reason? }。
// 本脚本为通用兜底：适配「标准 textarea / contenteditable 输入框」的站点。
// 站点特化脚本（chatgpt.js 等）= 复制本文件后覆盖 findComposer / findSendButton。

function findComposer() {
  return (
    document.querySelector('form textarea:not([readonly]):not([disabled])') ||
    document.querySelector('div textarea:not([readonly]):not([disabled])') ||
    document.querySelector('textarea[placeholder]') ||
    [...document.querySelectorAll('[contenteditable="true"]')]
      .filter((el) => !el.closest('[aria-hidden="true"]'))[0] ||
    null
  );
}

// React 受控组件：必须走原生 setter + input 事件，否则状态不更新
function setTextareaValue(el, text) {
  const setter = Object.getOwnPropertyDescriptor(
    HTMLTextAreaElement.prototype,
    'value',
  ).set;
  setter.call(el, text);
  el.dispatchEvent(new Event('input', { bubbles: true }));
}

// contenteditable（ProseMirror 类）：优先模拟 paste（兼容性最好），兜底 execCommand
function setContentEditable(el, text) {
  el.focus();
  const dt = new DataTransfer();
  dt.setData('text/plain', text);
  el.dispatchEvent(
    new ClipboardEvent('paste', {
      bubbles: true,
      cancelable: true,
      clipboardData: dt,
    }),
  );
  if ((el.textContent || '').trim()) return true;
  document.execCommand('insertText', false, text);
  return (el.textContent || '').trim().length > 0;
}

async function famaFill(text) {
  const el = findComposer();
  if (!el) return { ok: false, reason: 'composer-not-found' };
  if (el.tagName === 'TEXTAREA') setTextareaValue(el, text);
  else if (!setContentEditable(el, text))
    return { ok: false, reason: 'set-contenteditable-failed' };
  // verify：读回校验（取前 32 字符，容许站点对空白做规范化）
  const v = el.tagName === 'TEXTAREA' ? el.value : el.textContent;
  return v && v.includes(text.slice(0, 32).trim())
    ? { ok: true }
    : { ok: false, reason: 'verify-failed' };
}

function findSendButton(composer) {
  const scope = composer.closest('form') || document;
  return (
    scope.querySelector('button[data-testid*="send" i]:not([disabled])') ||
    scope.querySelector('button[aria-label*="send" i]:not([disabled])') ||
    scope.querySelector('button[aria-label*="发送"]:not([disabled])') ||
    scope.querySelector('button[type="submit"]:not([disabled])')
  );
}

async function famaSend() {
  const el = findComposer();
  if (!el) return { ok: false, reason: 'composer-not-found' };
  if (!(el.value || el.textContent || '').trim())
    return { ok: false, reason: 'empty-composer' };
  const btn = findSendButton(el);
  if (btn) {
    btn.click();
    return { ok: true, via: 'button' };
  }
  el.dispatchEvent(
    new KeyboardEvent('keydown', {
      key: 'Enter',
      code: 'Enter',
      bubbles: true,
      cancelable: true,
    }),
  );
  return { ok: true, via: 'enter' };
}
