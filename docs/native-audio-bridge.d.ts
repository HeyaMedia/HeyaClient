/** HeyaClient native audio bridge protocol v1. */

export type NativeAudioOutputMode = 'processed' | 'bit_perfect'
export type NativeAudioCrossfadeMode = 'gapless' | 'crossfade' | 'smart'
export type NativeAudioCrossfeedPreset = 'subtle' | 'natural' | 'strong'
export type NativeAudioTerminationReason =
  | 'ended' | 'stopped' | 'disposed' | 'failed' | 'native_crashed'
  | 'logged_out' | 'server_switched' | 'app_quit' | 'window_closed'
export type NativeAudioErrorCode =
  | 'invalid_request' | 'protocol_mismatch' | 'origin_not_allowed'
  | 'playback_grant_required' | 'backend_unavailable' | 'unknown_session'
  | 'renderer_stopping' | 'command_failed' | 'internal_error'

export interface NativeAudioProcessingSettings {
  replayGainEnabled: boolean
  eqEnabled: boolean
  eqBandsDb: number[]
  preampDb: number
  postgainDb: number
  limiterEnabled: boolean
  crossfeedEnabled: boolean
  crossfeedPreset: NativeAudioCrossfeedPreset
  dspOrder: Array<'equalizer' | 'crossfeed'>
  crossfadeMode: NativeAudioCrossfadeMode
  crossfadeSeconds: number
  albumAware: boolean
  visualizerEnabled: boolean
}

export interface NativeAudioCapabilities {
  protocolVersion: 1
  backend: 'heya-rust-audio'
  available: boolean
  gapless: boolean
  crossfade: boolean
  replayGain: boolean
  equalizer: boolean
  visualizer: boolean
  outputDeviceSelection: boolean
  preferredOutputMode: NativeAudioOutputMode
  bitPerfect: {
    available: boolean
    requiresExclusiveDevice: boolean
    unavailableReason?: string
  }
  unavailableReason?: string
}

export interface NativeAudioOutputDevice {
  deviceId: string
  label: string
  isDefault: boolean
}

export interface NativeAudioOutputDevices {
  devices: NativeAudioOutputDevice[]
  activeDeviceId: string | null
  followsSystemDefault: boolean
}

export interface NativeAudioTrackRequest {
  trackId: number
  durationSeconds: number
  albumKey: string
  formatHint?: string
  codec?: string
  sampleRateHz?: number
  bitDepth?: number
  channels?: number
  lossless?: boolean
  gainDb?: number
  media: {
    mediaUrl: string
    playbackGrant: string
    startPositionSeconds?: number
  }
}

export interface NativeAudioState {
  playing: boolean
  paused: boolean
  loading: boolean
  buffering: boolean
  ended: boolean
  positionSeconds: number
  durationSeconds: number
  volume: number
  muted: boolean
  currentTrackId: number | null
  startedTrackId: number | null
  endedTrackId: number | null
  outputMode: NativeAudioOutputMode
  bitPerfectActive: boolean
  sourceSampleRateHz: number | null
  sourceChannels: number | null
  outputSampleRateHz: number | null
  outputChannels: number | null
  outputDeviceId: string | null
  outputDeviceName: string | null
  resamplerActive: boolean
  dspActive: boolean
  error?: { code: NativeAudioErrorCode; message: string }
  terminationReason?: NativeAudioTerminationReason
}

export type NativeAudioCommand = {
  rendererSessionId: string
  commandId: string
} & (
  | { type: 'play' }
  | { type: 'pause' }
  | { type: 'seek'; positionSeconds: number }
  | { type: 'setVolume'; volume: number }
  | { type: 'setMuted'; muted: boolean }
  | { type: 'updateProcessing'; settings: NativeAudioProcessingSettings }
  | { type: 'stop' }
)

export interface NativeAudioCommandResult {
  rendererSessionId: string
  commandId: string
  commandSequence: number
  accepted: boolean
  duplicate: boolean
  error?: { code: NativeAudioErrorCode; message: string }
}

export interface HeyaNativeAudioBridge {
  readonly protocolVersion: 1
  getAudioCapabilities(): Promise<NativeAudioCapabilities>
  setAudioOutputMode(mode: NativeAudioOutputMode): Promise<NativeAudioCapabilities>
  getAudioOutputDevices(): Promise<NativeAudioOutputDevices>
  setAudioOutputDevice(deviceId: string | null): Promise<NativeAudioOutputDevices>
  loadAudio(request: {
    mode: NativeAudioOutputMode
    processing: NativeAudioProcessingSettings
    track: NativeAudioTrackRequest
  }): Promise<{ rendererSessionId: string; activeMode: NativeAudioOutputMode }>
  preloadNextAudio(request: {
    rendererSessionId: string
    commandId: string
    track: NativeAudioTrackRequest
  }): Promise<NativeAudioCommandResult>
  sendAudioCommand(command: NativeAudioCommand): Promise<NativeAudioCommandResult>
  subscribeAudioState(listener: (event: {
    protocolVersion: 1
    rendererSessionId: string
    stateRevision: number
    payload: NativeAudioState
  }) => void): () => void
  subscribeAudioVisualizer(listener: (event: {
    protocolVersion: 1
    rendererSessionId: string
    visualizerRevision: number
    samples: number[]
    frequencyBins: number[]
  }) => void): () => void
  disposeAudio(request: { rendererSessionId: string }): Promise<void>
}

declare global {
  interface Window {
    readonly __HEYA_NATIVE_AUDIO__?: Readonly<HeyaNativeAudioBridge>
  }
}

export {}
