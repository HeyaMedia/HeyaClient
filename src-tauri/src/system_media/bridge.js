(() => {
  'use strict'

  const protocolVersion = 1
  const commandName = '__HEYA_SYSTEM_MEDIA_COMMAND__'
  const readyEventName = 'heya:system-media:ready-v1'
  const commandEventName = 'heya:system-media:command-v1'
  const pageInstanceId = crypto.randomUUID()
  const commandListeners = new Set()
  let pageActive = true

  async function request(operation, payload, _keepalive = false) {
    if (!pageActive && operation !== 'owner-disappeared') {
      const error = new Error('The owning Heya page is no longer active.')
      error.code = 'unknown_session'
      throw error
    }

    const invoke = window.__TAURI_INTERNALS__?.invoke
    if (typeof invoke !== 'function') {
      const error = new Error('The system media transport is unavailable.')
      error.code = 'backend_unavailable'
      throw error
    }
    const result = await invoke(commandName, {
      request: { protocolVersion, pageInstanceId, operation, payload },
    })
    if (!result?.ok) {
      const error = new Error(result?.error?.message || 'System media request failed.')
      error.code = result?.error?.code || 'internal_error'
      throw error
    }
    return result.value
  }

  window.addEventListener(commandEventName, (event) => {
    if (event.detail?.pageInstanceId !== pageInstanceId) return
    for (const listener of [...commandListeners]) listener(event.detail.command)
  })

  const bridge = Object.freeze({
    protocolVersion,
    getSystemMediaCapabilities: () => request('capabilities', {}),
    updateSystemMedia: (snapshot) => request('update', snapshot),
    clearSystemMedia: (clearRequest) => request('clear', clearRequest),
    notifyTrackChanged: (notification) => request('notify-track-changed', notification),
    subscribeSystemMediaCommands: (listener) => {
      if (typeof listener !== 'function') throw new TypeError('listener must be a function')
      commandListeners.add(listener)
      return () => commandListeners.delete(listener)
    },
  })

  request('capabilities', {})
    .then((capabilities) => {
      if (!pageActive) return
      Object.defineProperty(window, '__HEYA_SYSTEM_MEDIA__', {
        value: bridge,
        configurable: false,
        enumerable: false,
        writable: false,
      })
      window.dispatchEvent(new CustomEvent(readyEventName, {
        detail: Object.freeze({ protocolVersion, capabilities }),
      }))
    })
    .catch((error) => {
      if (error?.code !== 'origin_not_allowed') {
        console.warn('[HeyaClient] system media bridge handshake failed', error)
      }
    })

  window.addEventListener('pagehide', () => {
    if (!pageActive) return
    pageActive = false
    void request('owner-disappeared', {}, true).catch(() => {})
  }, { once: true })
})()
