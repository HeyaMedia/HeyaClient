(() => {
  'use strict'

  const protocolVersion = 1
  const commandName = '__HEYA_APPLICATION_COMMAND__'
  const readyEventName = 'heya:application:ready-v1'
  const pageInstanceId = crypto.randomUUID()

  async function request(operation, payload = {}) {
    const invoke = window.__TAURI_INTERNALS__?.invoke
    if (typeof invoke !== 'function') {
      const error = new Error('The Heya application transport is unavailable.')
      error.code = 'backend_unavailable'
      throw error
    }
    const result = await invoke(commandName, {
      request: { protocolVersion, pageInstanceId, operation, payload },
    })
    if (!result?.ok) {
      const error = new Error(result?.error?.message || 'Heya application request failed.')
      error.code = result?.error?.code || 'internal_error'
      throw error
    }
    return result.value
  }

  const bridge = Object.freeze({
    protocolVersion,
    getApplicationCapabilities: () => request('capabilities'),
    getApplicationSnapshot: () => request('snapshot'),
    saveApplicationSettings: (settings) => request('save-settings', settings),
    checkForApplicationUpdate: () => request('check-for-update'),
    installApplicationUpdate: () => request('install-update'),
    installNativePlaybackRuntime: () => request('install-native-playback-runtime'),
    openServerPicker: () => request('open-server-picker'),
    resetServerSession: () => request('reset-server-session'),
    forgetServer: () => request('forget-server'),
  })

  request('capabilities')
    .then((capabilities) => {
      Object.defineProperty(window, '__HEYA_APPLICATION__', {
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
        console.warn('[HeyaClient] application bridge handshake failed', error)
      }
    })
})()
