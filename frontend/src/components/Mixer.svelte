<script lang="ts">
  import type { MixerTrack } from '../lib/midi-player'
  import type { StemTrack } from '../lib/stem-mixer'
  import InstrumentStrip from './InstrumentStrip.svelte'

  type MixerProps = {
    midiLoading: boolean
    mixerTracks: (MixerTrack | StemTrack)[]
    mixerRequested: boolean
    globalVolume: number
    trackLevels: Record<string, number>
    midiPlayerError: string
    stemsError: string | null
    onGlobalVolumeChange?: (volume: number) => void
    onTrackVolumeChange?: (trackId: string, volume: number) => void
    onTrackMuteToggle?: (trackId: string) => void
  }

  const noopGlobalVolumeChange = (_volume: number) => {}
  const noopTrackVolumeChange = (_trackId: string, _volume: number) => {}
  const noopTrackMuteToggle = (_trackId: string) => {}

  let {
    midiLoading,
    mixerTracks,
    mixerRequested,
    globalVolume,
    trackLevels,
    midiPlayerError,
    stemsError,
    onGlobalVolumeChange = noopGlobalVolumeChange,
    onTrackVolumeChange = noopTrackVolumeChange,
    onTrackMuteToggle = noopTrackMuteToggle,
  }: MixerProps = $props()

  const skeletonTrackNames = [
    { name: 'Violin I' },
    { name: 'Viola' },
    { name: 'Cello' },
    { name: 'Flute' },
    { name: 'Clarinet' },
    { name: 'Horn' },
    { name: 'Piano' },
  ]
  const skeletonStripCount = 24
  const skeletonOpacityStep = skeletonStripCount > 1 ? 0.8 / (skeletonStripCount - 1) : 0

  const skeletonTracks = Array.from({ length: skeletonStripCount }, (_, index) => ({
    name: skeletonTrackNames[index % skeletonTrackNames.length].name,
    opacity: 1 - index * skeletonOpacityStep,
  }))

  let showSkeleton = $derived(midiLoading || (!mixerRequested && mixerTracks.length === 0))

</script>

{#if showSkeleton}
  <div class="mixer-panel mixer-panel-loading">
    <div class="mixer-board mixer-board-loading">
      <InstrumentStrip
        name="All"
        volume={globalVolume}
        muted={false}
        disabled={true}
        showGauge={false}
        showMuteButton={false}
        highlight={true}
      />
      <div class="channel-divider"></div>
      <div class="skeleton-strip-list">
        {#each skeletonTracks as track, i}
          <InstrumentStrip
            name={track.name}
            volume={1}
            muted={false}
            opacity={track.opacity}
            skeleton={true}
            skeletonIndex={i + 1}
          />
        {/each}
      </div>
    </div>
  </div>
{:else if mixerTracks.length > 0}
  <div class="mixer-panel">
    <div class="mixer-board">
      <InstrumentStrip
        name="All"
        volume={globalVolume}
        muted={false}
        showGauge={false}
        showMuteButton={false}
        highlight={true}
        onVolumeChange={onGlobalVolumeChange}
      />

      <div class="channel-divider"></div>

      {#each mixerTracks as track}
        <InstrumentStrip
          name={track.name}
          volume={track.volume}
          muted={track.muted}
          level={trackLevels[track.id] ?? 0}
          onVolumeChange={(volume) => onTrackVolumeChange(track.id, volume)}
          onMuteToggle={() => onTrackMuteToggle(track.id)}
        />
      {/each}
    </div>
  </div>
{:else if mixerRequested}
  <p class="hint">
    Stem playback is not available yet.
    {#if stemsError}
      <br />
      <span>Stems: {stemsError}</span>
    {/if}
  </p>
{/if}

{#if midiPlayerError}
  <p class="status error">{midiPlayerError}</p>
{/if}
