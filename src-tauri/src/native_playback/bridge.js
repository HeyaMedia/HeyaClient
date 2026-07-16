(() => {
  'use strict'

  const protocolVersion = 1
  const endpointBase = '__HEYA_PLAYBACK_ENDPOINT__'
  const readyEventName = 'heya:native-playback:ready-v1'
  const stateEventName = 'heya:native-playback:state-v1'
  const diagnosticsEventName = 'heya:native-playback:diagnostics-v1'
  const pageInstanceId = crypto.randomUUID()
  const stateListeners = new Set()
  const diagnosticsListeners = new Set()
  let pageActive = true

  async function request(operation, payload, keepalive = false) {
    if (!pageActive && operation !== 'owner-disappeared') {
      const error = new Error('The owning Heya page is no longer active.')
      error.code = 'unknown_session'
      throw error
    }

    const response = await fetch(`${endpointBase}/v1/${operation}`, {
      method: 'POST',
      body: JSON.stringify({ protocolVersion, pageInstanceId, payload }),
      cache: 'no-store',
      credentials: 'omit',
      keepalive,
      redirect: 'error',
      referrerPolicy: 'no-referrer',
      headers: { 'Content-Type': 'text/plain;charset=UTF-8' },
    })
    const result = await response.json()
    if (!response.ok || !result?.ok) {
      const error = new Error(result?.error?.message || 'Native playback request failed.')
      error.code = result?.error?.code || 'internal_error'
      throw error
    }
    return result.value
  }

  function subscribe(listeners, listener) {
    if (typeof listener !== 'function') throw new TypeError('listener must be a function')
    listeners.add(listener)
    return () => listeners.delete(listener)
  }

  window.addEventListener(stateEventName, (event) => {
    if (event.detail?.pageInstanceId !== pageInstanceId) return
    for (const listener of [...stateListeners]) listener(event.detail.event)
  })

  window.addEventListener(diagnosticsEventName, (event) => {
    if (event.detail?.pageInstanceId !== pageInstanceId) return
    for (const listener of [...diagnosticsListeners]) listener(event.detail.event)
  })

  const bridge = Object.freeze({
    protocolVersion,
    getPlaybackCapabilities: () => request('capabilities', {}),
    loadPlayback: (loadRequest) => request('load', loadRequest),
    sendPlaybackCommand: (command) => request('command', command),
    subscribePlaybackState: (listener) => subscribe(stateListeners, listener),
    subscribePlaybackDiagnostics: (listener) => subscribe(diagnosticsListeners, listener),
    disposePlayback: (disposeRequest) => request('dispose', disposeRequest),
  })

  // The handshake is the authorization gate. The object is not installed at
  // all on the local bootstrap, settings page, or an unselected remote origin.
  request('capabilities', {})
    .then((capabilities) => {
      if (!pageActive) return
      Object.defineProperty(window, '__HEYA_NATIVE_PLAYBACK__', {
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
    if (!pageActive) return
    pageActive = false
    void request('owner-disappeared', {}, true).catch(() => {})
  }, { once: true })
})()
