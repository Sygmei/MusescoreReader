import { Midi } from '@tonejs/midi'
import * as Tone from 'tone'

export type MixerTrack = {
  id: string
  name: string
  instrumentName: string
  instrumentFamily: string
  channel: number
  noteCount: number
  volume: number
  muted: boolean
}

export type LoadedMidi = {
  duration: number
  tracks: MixerTrack[]
}

type EngineTrack = {
  id: string
  gain: Tone.Gain
  synth: Tone.PolySynth
  part: Tone.Part<any>
  volume: number
  muted: boolean
}

export class MidiMixerPlayer {
  private engineTracks = new Map<string, EngineTrack>()
  private duration = 0

  async loadFromUrl(url: string): Promise<LoadedMidi> {
    const response = await fetch(url)
    if (!response.ok) {
      throw new Error(`Unable to load MIDI file (${response.status})`)
    }

    const buffer = await response.arrayBuffer()
    const midi = new Midi(buffer)
    this.disposeTracks()
    this.resetTransport()

    const tracks = midi.tracks
      .filter((track) => track.notes.length > 0)
      .map((track, index) => this.createEngineTrack(track, index))

    this.duration = midi.duration

    return {
      duration: midi.duration,
      tracks: tracks.map((track) => track.mixerTrack),
    }
  }

  private createEngineTrack(track: Midi['tracks'][number], index: number): EngineTrack & {
    mixerTrack: MixerTrack
  } {
    const id = `${index}`
    const gain = new Tone.Gain(0.8).toDestination()
    const synth = new Tone.PolySynth(Tone.Synth, synthOptionsFor(track.instrument.family)).connect(
      gain,
    )
    const events = track.notes.map((note) => ({
      time: note.time,
      name: note.name,
      duration: note.duration,
      velocity: Math.max(note.velocity, 0.05),
    }))
    const part = new Tone.Part<any>(
      (time, note: { name: string; duration: number; velocity: number }) => {
        synth.triggerAttackRelease(note.name, note.duration, time, note.velocity)
      },
      events.map((event) => [event.time, event]),
    ).start(0)

    this.engineTracks.set(id, {
      id,
      gain,
      synth,
      part,
      volume: 0.8,
      muted: false,
    })

    const mixerTrack = {
      id,
      name:
        track.name?.trim() ||
        track.instrument.name?.trim() ||
        `${capitalize(track.instrument.family || 'instrument')} ${index + 1}`,
      instrumentName: track.instrument.name || 'Unknown instrument',
      instrumentFamily: track.instrument.family || 'other',
      channel: track.channel,
      noteCount: track.notes.length,
      volume: 0.8,
      muted: false,
    }

    return {
      id,
      gain,
      synth,
      part,
      volume: 0.8,
      muted: false,
      mixerTrack,
    }
  }

  async start(): Promise<void> {
    await Tone.start()
    const transport = Tone.getTransport()
    if (transport.seconds >= this.duration && this.duration > 0) {
      transport.seconds = 0
    }
    transport.start()
  }

  async play(): Promise<void> {
    return this.start()
  }

  pause(): void {
    Tone.getTransport().pause()
  }

  stop(): void {
    const transport = Tone.getTransport()
    transport.stop()
    transport.seconds = 0
  }

  seek(seconds: number): void {
    Tone.getTransport().seconds = clamp(seconds, 0, this.duration)
  }

  setTrackVolume(trackId: string, volume: number): void {
    const track = this.engineTracks.get(trackId)
    if (!track) {
      return
    }

    track.volume = clamp(volume, 0, 2)
    track.gain.gain.rampTo(track.muted ? 0 : track.volume, 0.05)
  }

  setTrackMuted(trackId: string, muted: boolean): void {
    const track = this.engineTracks.get(trackId)
    if (!track) {
      return
    }

    track.muted = muted
    track.gain.gain.rampTo(track.muted ? 0 : track.volume, 0.05)
  }

  getCurrentTime(): number {
    return Math.min(Tone.getTransport().seconds, this.duration)
  }

  getDuration(): number {
    return this.duration
  }

  isPlaying(): boolean {
    return Tone.getTransport().state === 'started'
  }

  async dispose(): Promise<void> {
    this.disposeTracks()
    this.resetTransport()
  }

  private disposeTracks(): void {
    for (const track of this.engineTracks.values()) {
      track.part.dispose()
      track.synth.releaseAll()
      track.synth.dispose()
      track.gain.dispose()
    }
    this.engineTracks.clear()
  }

  private resetTransport(): void {
    const transport = Tone.getTransport()
    transport.stop()
    transport.cancel(0)
    transport.seconds = 0
  }
}

function capitalize(value: string): string {
  return value.charAt(0).toUpperCase() + value.slice(1)
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(Math.max(value, min), max)
}

function synthOptionsFor(family: string) {
  switch (family) {
    case 'piano':
      return {
        oscillator: { type: 'triangle' as const },
        envelope: { attack: 0.01, decay: 0.2, sustain: 0.35, release: 0.8 },
      }
    case 'strings':
      return {
        oscillator: { type: 'sawtooth' as const },
        envelope: { attack: 0.03, decay: 0.15, sustain: 0.5, release: 1.2 },
      }
    case 'brass':
      return {
        oscillator: { type: 'square' as const },
        envelope: { attack: 0.02, decay: 0.12, sustain: 0.45, release: 0.7 },
      }
    case 'woodwind':
      return {
        oscillator: { type: 'triangle' as const },
        envelope: { attack: 0.02, decay: 0.08, sustain: 0.4, release: 0.5 },
      }
    case 'guitar':
      return {
        oscillator: { type: 'triangle' as const },
        envelope: { attack: 0.005, decay: 0.25, sustain: 0.15, release: 0.6 },
      }
    case 'drums':
      return {
        oscillator: { type: 'square' as const },
        envelope: { attack: 0.001, decay: 0.15, sustain: 0.05, release: 0.12 },
      }
    default:
      return {
        oscillator: { type: 'sine' as const },
        envelope: { attack: 0.01, decay: 0.1, sustain: 0.3, release: 0.4 },
      }
  }
}
