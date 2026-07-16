(() => {
  'use strict'

  const protocolVersion = 1
  const commandName = '__HEYA_NATIVE_WINDOW_COMMAND__'
  const readyEventName = 'heya:native-window:ready-v1'
  const pageInstanceId = crypto.randomUUID()
  let pageActive = true

  async function request(operation, payload = {}) {
    if (!pageActive) {
      const error = new Error('The owning Heya page is no longer active.')
      error.code = 'unknown_session'
      throw error
    }
    const invoke = window.__TAURI_INTERNALS__?.invoke
    if (typeof invoke !== 'function') {
      const error = new Error('The native window transport is unavailable.')
      error.code = 'backend_unavailable'
      throw error
    }
    const result = await invoke(commandName, {
      request: { protocolVersion, pageInstanceId, operation, payload },
    })
    if (!result?.ok) {
      const error = new Error(result?.error?.message || 'Native window request failed.')
      error.code = result?.error?.code || 'internal_error'
      throw error
    }
    return result.value
  }

  const bridge = Object.freeze({
    protocolVersion,
    getWindowCapabilities: () => request('capabilities'),
    getWindowState: () => request('state'),
    minimize: () => request('minimize'),
    toggleMaximize: () => request('toggle-maximize'),
    startDragging: () => request('start-dragging'),
    setNativeControlsVisible: (visible) => request('set-native-controls-visible', { visible }),
    close: () => request('close'),
  })

  request('capabilities')
    .then((capabilities) => {
      if (!pageActive) return
      Object.defineProperty(window, '__HEYA_NATIVE_WINDOW__', {
        value: bridge,
        configurable: false,
        enumerable: false,
        writable: false,
      })
      window.dispatchEvent(new CustomEvent(readyEventName, {
        detail: Object.freeze({ protocolVersion, capabilities }),
      }))
    })
    .catch(() => {})

  window.addEventListener('pagehide', () => {
    pageActive = false
  }, { once: true })
})()
