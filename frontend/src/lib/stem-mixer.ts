export type StemTrack = {
  id: string
  name: string
  instrumentName: string
  volume: number
  muted: boolean
}

export type LoadedStems = {
  duration: number
  tracks: StemTrack[]
}

type StemSource = {
  id: string
  name: string
  instrumentName: string
  fullStemUrl: string
  durationSeconds: number
}

type InternalTrack = {
  meta: StemSource
  element: HTMLAudioElement
  source: MediaElementAudioSourceNode
  gain: GainNode
  analyser: AnalyserNode
  analyserData: Uint8Array<ArrayBuffer>
  volume: number
  muted: boolean
}

export class StemMixerPlayer {
  private context: AudioContext | null = null
  private tracks = new Map<string, InternalTrack>()
  private _duration = 0
  private _levelMultiplier = 15
  private _isPlaying = false
  private _playbackOffset = 0

  async loadStems(stems: StemSource[]): Promise<LoadedStems> {
    await this.dispose()

    this.context = new AudioContext()
    this._duration = stems.reduce((max, stem) => Math.max(max, stem.durationSeconds), 0)

    for (const stem of stems) {
      const element = new Audio()
      element.src = stem.fullStemUrl
      element.preload = 'auto'
      element.crossOrigin = 'anonymous'

      const source = this.context.createMediaElementSource(element)
      const gain = this.context.createGain()
      gain.gain.value = 1
      const analyser = this.context.createAnalyser()
      analyser.fftSize = 1024
      analyser.smoothingTimeConstant = 0.6

      source.connect(gain)
      gain.connect(analyser)
      analyser.connect(this.context.destination)

      this.tracks.set(stem.id, {
        meta: stem,
        element,
        source,
        gain,
        analyser,
        analyserData: new Uint8Array(analyser.fftSize) as Uint8Array<ArrayBuffer>,
        volume: 1,
        muted: false,
      })
    }

    return {
      duration: this._duration,
      tracks: stems.map((stem) => ({
        id: stem.id,
        name: stem.name,
        instrumentName: stem.instrumentName,
        volume: 1,
        muted: false,
      })),
    }
  }

  async play(): Promise<void> {
    if (!this.context) return
    if (this.context.state === 'suspended') {
      await this.context.resume()
    }

    const seekTime = this._playbackOffset
    for (const track of this.tracks.values()) {
      if (Math.abs(track.element.currentTime - seekTime) > 0.03) {
        track.element.currentTime = seekTime
      }
    }

    const starts = [...this.tracks.values()].map(async (track) => {
      try {
        await track.element.play()
      } catch (error) {
        throw error instanceof Error ? error : new Error('Unable to start stem playback')
      }
    })

    await Promise.all(starts)
    this._isPlaying = true
  }

  pause(): void {
    if (!this._isPlaying) return
    this._playbackOffset = this.getCurrentTime()
    this._isPlaying = false
    for (const track of this.tracks.values()) {
      track.element.pause()
    }
  }

  stop(): void {
    this._isPlaying = false
    this._playbackOffset = 0
    for (const track of this.tracks.values()) {
      track.element.pause()
      track.element.currentTime = 0
    }
  }

  seek(seconds: number): void {
    const clamped = Math.max(0, Math.min(seconds, this._duration))
    this._playbackOffset = clamped
    for (const track of this.tracks.values()) {
      track.element.currentTime = clamped
    }
  }

  getCurrentTime(): number {
    if (this.tracks.size === 0) return this._playbackOffset
    if (!this._isPlaying) return this._playbackOffset

    // Some rendered stems are shorter than the full arrangement because the
    // backend trims trailing silence. Using the first inserted track as the
    // transport clock makes the UI snap backward as soon as that stem ends,
    // even while longer stems are still playing.
    let latestTime = this._playbackOffset
    for (const track of this.tracks.values()) {
      latestTime = Math.max(latestTime, track.element.currentTime || 0)
    }

    return Math.min(latestTime, this._duration)
  }

  getDuration(): number {
    return this._duration
  }

  isPlaying(): boolean {
    return this._isPlaying
  }

  getLevel(trackId: string): number {
    const track = this.tracks.get(trackId)
    if (!track) return 0
    track.analyser.getByteTimeDomainData(track.analyserData)

    let sumSquares = 0
    const data = track.analyserData
    for (let index = 0; index < data.length; index += 1) {
      const centered = (data[index] - 128) / 128
      sumSquares += centered * centered
    }

    const rms = Math.sqrt(sumSquares / data.length)
    return Math.min(1, rms * this._levelMultiplier)
  }

  setTrackVolume(trackId: string, volume: number): void {
    const track = this.tracks.get(trackId)
    if (!track) return
    track.volume = Math.max(0, Math.min(volume, 1))
    this._applyTrackGain(track)
  }

  setTrackMuted(trackId: string, muted: boolean): void {
    const track = this.tracks.get(trackId)
    if (!track) return
    track.muted = muted
    this._applyTrackGain(track)
  }

  setLevelMultiplier(multiplier: number): void {
    this._levelMultiplier = Math.max(1, multiplier)
  }

  async dispose(): Promise<void> {
    this._isPlaying = false
    this._playbackOffset = 0

    for (const track of this.tracks.values()) {
      track.element.pause()
      track.element.removeAttribute('src')
      track.element.load()
      track.source.disconnect()
      track.gain.disconnect()
      track.analyser.disconnect()
    }

    this.tracks.clear()
    this._duration = 0

    if (this.context) {
      await this.context.close()
      this.context = null
    }
  }

  private _applyTrackGain(track: InternalTrack): void {
    track.gain.gain.value = track.muted ? 0 : track.volume
  }
}
