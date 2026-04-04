/**
 * Chunked stem mixer.
 *
 * Each stem is split into identical time-aligned Ogg/Opus chunks on the backend.
 * The player fetches and decodes chunks on demand, then schedules them on a shared
 * AudioContext clock so the stems stay tightly synchronized without downloading
 * every full file up front.
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

type StemSource = {
  id: string
  name: string
  instrumentName: string
  chunkUrlTemplate: string
  chunkCount: number
  chunkDurationSeconds: number
  durationSeconds: number
}

type InternalTrack = {
  meta: StemSource
  gain: GainNode
  analyser: AnalyserNode
  analyserData: Uint8Array<ArrayBuffer>
  volume: number
  muted: boolean
  chunks: Map<number, AudioBuffer>
  chunkLoads: Map<number, Promise<AudioBuffer>>
  scheduledSources: Map<number, AudioBufferSourceNode>
}

const INITIAL_PRELOAD_CHUNKS = 2
const SCHEDULE_HORIZON_SECONDS = 10
const SCHEDULE_TICK_MS = 500

export class StemMixerPlayer {
  private context: AudioContext | null = null
  private tracks = new Map<string, InternalTrack>()
  private _duration = 0
  private _levelMultiplier = 15
  private _isPlaying = false
  private _playbackStartOffset = 0
  private _playbackStartCtxTime = 0
  private _scheduleTimer: number | null = null
  private _playbackGeneration = 0

  async loadStems(stems: StemSource[]): Promise<LoadedStems> {
    await this.dispose()

    this.context = new AudioContext()
    this._duration = stems.reduce((max, stem) => Math.max(max, stem.durationSeconds), 0)

    for (const stem of stems) {
      const gain = this.context.createGain()
      gain.gain.value = 1.0
      const analyser = this.context.createAnalyser()
      analyser.fftSize = 1024
      analyser.smoothingTimeConstant = 0.6
      gain.connect(analyser)
      analyser.connect(this.context.destination)

      this.tracks.set(stem.id, {
        meta: stem,
        gain,
        analyser,
        analyserData: new Uint8Array(new ArrayBuffer(analyser.fftSize)),
        volume: 1.0,
        muted: false,
        chunks: new Map(),
        chunkLoads: new Map(),
        scheduledSources: new Map(),
      })
    }

    await Promise.all(
      stems.map(async (stem) => {
        await this._ensureChunkLoaded(stem.id, 0)
      }),
    )

    return {
      duration: this._duration,
      tracks: stems.map((stem) => ({
        id: stem.id,
        trackIndex: Number(stem.id),
        name: stem.name,
        instrumentName: stem.instrumentName,
        volume: 1.0,
        muted: false,
      })),
    }
  }

  async play(): Promise<void> {
    if (!this.context) return
    if (this.context.state === 'suspended') {
      await this.context.resume()
    }

    this._playbackGeneration += 1
    const generation = this._playbackGeneration
    const offset = this._playbackStartOffset
    const startChunk = this._chunkIndexForOffset(offset)

    await this._preloadChunkWindow(startChunk)
    this._stopAllSources()

    this._playbackStartCtxTime = this.context.currentTime + 0.1
    this._playbackStartOffset = offset
    this._isPlaying = true

    await this._scheduleChunksThrough(generation, offset + SCHEDULE_HORIZON_SECONDS)
    this._armScheduler(generation)
  }

  pause(): void {
    if (!this._isPlaying) return
    this._playbackStartOffset = this.getCurrentTime()
    this._isPlaying = false
    this._clearScheduler()
    this._stopAllSources()
  }

  stop(): void {
    this._isPlaying = false
    this._playbackStartOffset = 0
    this._clearScheduler()
    this._stopAllSources()
  }

  seek(seconds: number): void {
    const clamped = Math.max(0, Math.min(seconds, this._duration))
    const wasPlaying = this._isPlaying
    this._isPlaying = false
    this._clearScheduler()
    this._stopAllSources()
    this._playbackStartOffset = clamped
    if (wasPlaying) {
      void this.play()
    }
  }

  getCurrentTime(): number {
    if (!this.context) return this._playbackStartOffset
    if (!this._isPlaying) return this._playbackStartOffset
    const elapsed = this.context.currentTime - this._playbackStartCtxTime
    return Math.min(this._playbackStartOffset + Math.max(0, elapsed), this._duration)
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
    let sum = 0
    for (let i = 0; i < track.analyserData.length; i += 1) {
      const value = (track.analyserData[i] - 128) / 128
      sum += value * value
    }
    return Math.min(1, Math.sqrt(sum / track.analyserData.length) * this._levelMultiplier)
  }

  setLevelMultiplier(value: number): void {
    this._levelMultiplier = Math.max(1, value)
  }

  setTrackVolume(trackId: string, volume: number): void {
    const track = this.tracks.get(trackId)
    if (!track || !this.context) return
    track.volume = Math.max(0, volume)
    if (!track.muted) {
      track.gain.gain.setTargetAtTime(track.volume, this.context.currentTime, 0.01)
    }
  }

  setTrackMuted(trackId: string, muted: boolean): void {
    const track = this.tracks.get(trackId)
    if (!track || !this.context) return
    track.muted = muted
    track.gain.gain.setTargetAtTime(muted ? 0 : track.volume, this.context.currentTime, 0.01)
  }

  async dispose(): Promise<void> {
    this._isPlaying = false
    this._playbackStartOffset = 0
    this._playbackGeneration += 1
    this._clearScheduler()
    this._stopAllSources()
    for (const track of this.tracks.values()) {
      track.gain.disconnect()
      track.analyser.disconnect()
      track.chunks.clear()
      track.chunkLoads.clear()
    }
    this.tracks.clear()
    this._duration = 0
    if (this.context) {
      await this.context.close()
      this.context = null
    }
  }

  private _chunkIndexForOffset(offsetSeconds: number): number {
    const firstTrack = this.tracks.values().next().value as InternalTrack | undefined
    if (!firstTrack) return 0
    return Math.max(0, Math.floor(offsetSeconds / firstTrack.meta.chunkDurationSeconds))
  }

  private async _preloadChunkWindow(startChunk: number): Promise<void> {
    const chunkIndices = new Set<number>()
    for (let index = startChunk; index < startChunk + INITIAL_PRELOAD_CHUNKS; index += 1) {
      chunkIndices.add(index)
    }

    await Promise.all(
      [...this.tracks.values()].flatMap((track) =>
        [...chunkIndices]
          .filter((chunkIndex) => chunkIndex < track.meta.chunkCount)
          .map((chunkIndex) => this._ensureChunkLoaded(track.meta.id, chunkIndex)),
      ),
    )
  }

  private _chunkUrl(meta: StemSource, chunkIndex: number): string {
    return meta.chunkUrlTemplate.replace('__CHUNK_INDEX__', String(chunkIndex))
  }

  private async _ensureChunkLoaded(trackId: string, chunkIndex: number): Promise<AudioBuffer> {
    const track = this.tracks.get(trackId)
    if (!track || !this.context) {
      throw new Error('Stem mixer is not initialized')
    }
    if (chunkIndex < 0 || chunkIndex >= track.meta.chunkCount) {
      throw new Error(`Chunk ${chunkIndex} is out of range for "${track.meta.name}"`)
    }

    const existing = track.chunks.get(chunkIndex)
    if (existing) {
      return existing
    }

    const inFlight = track.chunkLoads.get(chunkIndex)
    if (inFlight) {
      return inFlight
    }

    const load = (async () => {
      const response = await fetch(this._chunkUrl(track.meta, chunkIndex))
      if (!response.ok) {
        throw new Error(
          `Failed to fetch chunk ${chunkIndex} for "${track.meta.name}": HTTP ${response.status}`,
        )
      }
      const buffer = await response.arrayBuffer()
      const audioBuffer = await this.context!.decodeAudioData(buffer)
      track.chunks.set(chunkIndex, audioBuffer)
      track.chunkLoads.delete(chunkIndex)
      return audioBuffer
    })()

    track.chunkLoads.set(chunkIndex, load)
    return load
  }

  private _armScheduler(generation: number): void {
    this._clearScheduler()
    this._scheduleTimer = window.setTimeout(() => {
      void this._schedulerTick(generation)
    }, SCHEDULE_TICK_MS)
  }

  private async _schedulerTick(generation: number): Promise<void> {
    if (!this._isPlaying || generation !== this._playbackGeneration) {
      return
    }

    const current = this.getCurrentTime()
    if (current >= this._duration - 0.02) {
      this._isPlaying = false
      this._clearScheduler()
      return
    }

    await this._scheduleChunksThrough(generation, current + SCHEDULE_HORIZON_SECONDS)

    if (this._isPlaying && generation === this._playbackGeneration) {
      this._armScheduler(generation)
    }
  }

  private async _scheduleChunksThrough(generation: number, horizonOffsetSeconds: number): Promise<void> {
    if (!this.context || generation !== this._playbackGeneration) return

    const currentOffset = this.getCurrentTime()

    for (const track of this.tracks.values()) {
      const startChunk = Math.max(
        0,
        Math.floor(currentOffset / track.meta.chunkDurationSeconds),
      )
      const endChunk = Math.min(
        track.meta.chunkCount - 1,
        Math.floor(horizonOffsetSeconds / track.meta.chunkDurationSeconds),
      )

      for (let chunkIndex = startChunk; chunkIndex <= endChunk; chunkIndex += 1) {
        if (track.scheduledSources.has(chunkIndex)) {
          continue
        }

        const audioBuffer = await this._ensureChunkLoaded(track.meta.id, chunkIndex)
        if (generation !== this._playbackGeneration || !this.context) {
          return
        }

        const chunkStartOffset = chunkIndex * track.meta.chunkDurationSeconds
        const desiredStartTime =
          this._playbackStartCtxTime + (chunkStartOffset - this._playbackStartOffset)

        let actualStartTime = desiredStartTime
        let offsetIntoChunk =
          chunkIndex === startChunk
            ? Math.max(0, currentOffset - chunkStartOffset)
            : 0

        const minimumLeadTime = this.context.currentTime + 0.02
        if (actualStartTime < minimumLeadTime) {
          const lateBy = minimumLeadTime - actualStartTime
          actualStartTime = minimumLeadTime
          offsetIntoChunk += lateBy
        }

        if (offsetIntoChunk >= audioBuffer.duration - 0.005) {
          continue
        }

        const source = this.context.createBufferSource()
        source.buffer = audioBuffer
        source.connect(track.gain)
        source.start(actualStartTime, offsetIntoChunk)
        source.onended = () => {
          if (track.scheduledSources.get(chunkIndex) === source) {
            track.scheduledSources.delete(chunkIndex)
          }
        }
        track.scheduledSources.set(chunkIndex, source)

        const nextChunkIndex = chunkIndex + INITIAL_PRELOAD_CHUNKS
        if (nextChunkIndex < track.meta.chunkCount) {
          void this._ensureChunkLoaded(track.meta.id, nextChunkIndex)
        }
      }
    }
  }

  private _clearScheduler(): void {
    if (this._scheduleTimer !== null) {
      window.clearTimeout(this._scheduleTimer)
      this._scheduleTimer = null
    }
  }

  private _stopAllSources(): void {
    for (const track of this.tracks.values()) {
      for (const source of track.scheduledSources.values()) {
        try {
          source.stop()
        } catch {
          // ignored
        }
        source.disconnect()
      }
      track.scheduledSources.clear()
    }
  }
}
