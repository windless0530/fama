// Fama 站点脚本：ChatGPT（chatgpt.com）
// @version 1
// 选择器为历史经验值，M0 真机校准（design.md §2.4 表格）
//
// 输入框：ProseMirror contenteditable（#prompt-textarea）
// 写值策略：模拟 paste 事件（ProseMirror 对 paste 处理最完整）

function findComposer() {
  return (
    document.querySelector('#prompt-textarea') ||
    document.querySelector('form [contenteditable="true"]') ||
    [...document.querySelectorAll('[contenteditable="true"]')]
      .filter((el) => !el.closest('[aria-hidden="true"]'))[0] ||
    null
  );
}

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
  if (el.tagName === 'TEXTAREA') {
    const setter = Object.getOwnPropertyDescriptor(
      HTMLTextAreaElement.prototype,
      'value',
    ).set;
    setter.call(el, text);
    el.dispatchEvent(new Event('input', { bubbles: true }));
  } else if (!setContentEditable(el, text)) {
    return { ok: false, reason: 'set-contenteditable-failed' };
  }
  const v = el.tagName === 'TEXTAREA' ? el.value : el.textContent;
  return v && v.includes(text.slice(0, 32).trim())
    ? { ok: true }
    : { ok: false, reason: 'verify-failed' };
}

function findSendButton(composer) {
  const scope = composer.closest('form') || document;
  return (
    scope.querySelector('button[data-testid="send-button"]') ||
    scope.querySelector('button[data-testid="stop-button"]') ||
    scope.querySelector('button[aria-label="Send prompt"]') ||
    scope.querySelector('button[aria-label*="send" i]:not([disabled])') ||
    scope.querySelector('button[type="submit"]:not([disabled])')
  );
}

async function famaSend() {
  const el = findComposer();
  if (!el) return { ok: false, reason: 'composer-not-found' };
  if (!(el.value || el.textContent || '').trim())
    return { ok: false, reason: 'empty-composer' };
  const btn = findSendButton(el);
  if (btn && btn.getAttribute('data-testid') !== 'stop-button') {
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
