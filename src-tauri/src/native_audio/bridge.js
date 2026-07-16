(() => {
  'use strict'

  const protocolVersion = 1
  const endpointBase = '__HEYA_AUDIO_ENDPOINT__'
  const readyEventName = 'heya:native-audio:ready-v1'
  const stateEventName = 'heya:native-audio:state-v1'
  const visualizerEventName = 'heya:native-audio:visualizer-v1'
  const pageInstanceId = crypto.randomUUID()
  const stateListeners = new Set()
  const visualizerListeners = new Set()
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
      const error = new Error(result?.error?.message || 'Native audio request failed.')
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

  window.addEventListener(visualizerEventName, (event) => {
    if (event.detail?.pageInstanceId !== pageInstanceId) return
    for (const listener of [...visualizerListeners]) listener(event.detail.event)
  })

  const bridge = Object.freeze({
    protocolVersion,
    getAudioCapabilities: () => request('capabilities', {}),
    setAudioOutputMode: (mode) => request('output-mode', { mode }),
    getAudioOutputDevices: () => request('output-devices', {}),
    setAudioOutputDevice: (deviceId) => request('output-device', { deviceId }),
    loadAudio: (loadRequest) => request('load', loadRequest),
    preloadNextAudio: (preloadRequest) => request('preload', preloadRequest),
    sendAudioCommand: (command) => request('command', command),
    subscribeAudioState: (listener) => subscribe(stateListeners, listener),
    subscribeAudioVisualizer: (listener) => subscribe(visualizerListeners, listener),
    disposeAudio: (disposeRequest) => request('dispose', disposeRequest),
  })

  request('capabilities', {})
    .then((capabilities) => {
      if (!pageActive) return
      Object.defineProperty(window, '__HEYA_NATIVE_AUDIO__', {
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
      // The script also runs on HeyaClient's local bootstrap page, where an
      // origin rejection is expected. Unexpected failures on the selected
      // Heya origin must stay visible so a broken bridge does not look like an
      // unexplained WebAudio fallback. Capability requests carry no secrets.
      if (error?.code !== 'origin_not_allowed') {
        console.warn('[HeyaClient] native audio bridge handshake failed', error)
      }
    })

  window.addEventListener('pagehide', () => {
    if (!pageActive) return
    pageActive = false
    void request('owner-disappeared', {}, true).catch(() => {})
  }, { once: true })
})()
