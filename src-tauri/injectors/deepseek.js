// Fama 站点脚本：DeepSeek（chat.deepseek.com）
// @version 1
// TODO(M0 校准)：确认 composer 与发送按钮选择器；当前为通用兜底逻辑

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

function setTextareaValue(el, text) {
  const setter = Object.getOwnPropertyDescriptor(
    HTMLTextAreaElement.prototype,
    'value',
  ).set;
  setter.call(el, text);
  el.dispatchEvent(new Event('input', { bubbles: true }));
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
  if (el.tagName === 'TEXTAREA') setTextareaValue(el, text);
  else if (!setContentEditable(el, text))
    return { ok: false, reason: 'set-contenteditable-failed' };
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
