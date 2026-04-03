/**
 * StemMixerPlayer — plays per-instrument OGG stems rendered by the backend
 * using the Web Audio API.  Each stem gets its own GainNode so volume and
 * muting are independent.  All HTMLAudioElements are started in one
 * microtask to stay closely synchronised.
 */

export type StemTrack = {
  id: string
  trackIndex: number
  name: string
  instrumentName: string
  volume: number
  muted: boolean
}

export type LoadedStems = {
  tracks: StemTrack[]
  duration: number
}

type InternalTrack = {
  el: HTMLAudioElement
  source: MediaElementAudioSourceNode
  gain: GainNode
  analyser: AnalyserNode
  analyserData: Uint8Array
  volume: number
  muted: boolean
}

export class StemMixerPlayer {
  private context: AudioContext | null = null
  private tracks = new Map<string, InternalTrack>()
  private _duration = 0
  private _levelMultiplier = 6

  async loadStems(
    stems: Array<{ id: string; name: string; instrumentName: string; streamUrl: string }>,
  ): Promise<LoadedStems> {
    await this.dispose()

    this.context = new AudioContext()
    const loadPromises = stems.map((stem) => this.loadOneStem(stem))

    await Promise.all(loadPromises)

    const result: StemTrack[] = stems
      .filter((s) => this.tracks.has(s.id))
      .map((s) => ({
        id: s.id,
        trackIndex: Number(s.id),
        name: s.name,
        instrumentName: s.instrumentName,
        volume: 0.5,
        muted: false,
      }))

    return { tracks: result, duration: this._duration }
  }

  private async loadOneStem(stem: {
    id: string
    name: string
    streamUrl: string
  }): Promise<void> {
    const el = new Audio()
    el.crossOrigin = 'anonymous'
    el.preload = 'auto'
    el.src = stem.streamUrl

    await new Promise<void>((resolve, reject) => {
      el.addEventListener('loadedmetadata', () => resolve(), { once: true })
      el.addEventListener(
        'error',
        () => reject(new Error(`Failed to load stem "${stem.name}"`)),
        { once: true },
      )
    })

    if (el.duration > this._duration) {
      this._duration = el.duration
    }

    const source = this.context!.createMediaElementSource(el)
    const gain = this.context!.createGain()
    gain.gain.value = 0.5
    const analyser = this.context!.createAnalyser()
    analyser.fftSize = 1024
    analyser.smoothingTimeConstant = 0.6
    source.connect(gain)
    gain.connect(analyser)
    analyser.connect(this.context!.destination)

    this.tracks.set(stem.id, {
      el, source, gain, analyser,
      analyserData: new Uint8Array(analyser.fftSize),
      volume: 0.5, muted: false,
    })
  }

  async play(): Promise<void> {
    if (!this.context) return
    if (this.context.state === 'suspended') {
      await this.context.resume()
    }
    await Promise.all([...this.tracks.values()].map((t) => t.el.play()))
  }

  pause(): void {
    for (const t of this.tracks.values()) {
      t.el.pause()
    }
  }

  stop(): void {
    for (const t of this.tracks.values()) {
      t.el.pause()
      t.el.currentTime = 0
    }
  }

  seek(seconds: number): void {
    for (const t of this.tracks.values()) {
      t.el.currentTime = Math.max(0, Math.min(seconds, this._duration))
    }
  }

  getCurrentTime(): number {
    const first = this.tracks.values().next().value as InternalTrack | undefined
    return first?.el.currentTime ?? 0
  }

  getDuration(): number {
    return this._duration
  }

  isPlaying(): boolean {
    const first = this.tracks.values().next().value as InternalTrack | undefined
    return first ? !first.el.paused : false
  }

  /** Returns a 0–1 RMS level for the given track, suitable for a VU meter. */
  getLevel(trackId: string): number {
    const t = this.tracks.get(trackId)
    if (!t) return 0
    t.analyser.getByteTimeDomainData(t.analyserData)
    let sum = 0
    for (let i = 0; i < t.analyserData.length; i++) {
      const v = (t.analyserData[i] - 128) / 128
      sum += v * v
    }
    return Math.min(1, Math.sqrt(sum / t.analyserData.length) * this._levelMultiplier)
  }

  setLevelMultiplier(value: number): void {
    this._levelMultiplier = Math.max(1, value)
  }

  setTrackVolume(trackId: string, volume: number): void {
    const t = this.tracks.get(trackId)
    if (!t || !this.context) return
    t.volume = Math.max(0, Math.min(1, volume))
    if (!t.muted) {
      t.gain.gain.setTargetAtTime(t.volume, this.context.currentTime, 0.01)
    }
  }

  setTrackMuted(trackId: string, muted: boolean): void {
    const t = this.tracks.get(trackId)
    if (!t || !this.context) return
    t.muted = muted
    t.gain.gain.setTargetAtTime(
      muted ? 0 : t.volume,
      this.context.currentTime,
      0.01,
    )
  }

  async dispose(): Promise<void> {
    this.stop()
    for (const t of this.tracks.values()) {
      t.source.disconnect()
      t.gain.disconnect()
      t.analyser.disconnect()
      t.el.src = ''
    }
    this.tracks.clear()
    this._duration = 0
    if (this.context) {
      await this.context.close()
      this.context = null
    }
  }
}
