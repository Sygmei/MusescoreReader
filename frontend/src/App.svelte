<script lang="ts">
  import { onDestroy, onMount, tick } from 'svelte'
  import {
    fetchPublicMusic,
    fetchStems,
    listMusics,
    login,
    retryRender,
    updatePublicId,
    uploadMusic,
    type AdminMusic,
    type PublicMusic,
    type Stem,
  } from './lib/api'
  import { MidiMixerPlayer, type MixerTrack } from './lib/midi-player'
  import { StemMixerPlayer, type StemTrack } from './lib/stem-mixer'
  import { ScoreViewer } from './lib/score-viewer'

  const storedPassword =
    typeof window !== 'undefined' ? window.localStorage.getItem('admin-password') ?? '' : ''
  const path = typeof window !== 'undefined' ? window.location.pathname : '/'
  const routeMatch = path.match(/^\/listen\/([^/]+)$/)
  const publicAccessKey = routeMatch ? decodeURIComponent(routeMatch[1]) : null
  const isPublicRoute = publicAccessKey !== null

  let adminPassword = storedPassword
  let adminLoggedIn = false
  let adminLoading = false
  let adminError = ''
  let adminSuccess = ''
  let uploadTitle = ''
  let uploadPublicId = ''
  let selectedFile: File | null = null
  let uploadBusy = false
  let musics: AdminMusic[] = []
  let editPublicIds: Record<string, string> = {}
  let savingIdFor = ''
  let retryingFor = ''

  let publicMusic: PublicMusic | null = null
  let publicLoading = false
  let publicError = ''

  let scoreViewer: ScoreViewer | null = null
  let scoreContainer: HTMLElement | null = null
  let scoreLoading = false
  let scoreLoaded = false
  let scoreError = ''

  let midiPlayer: MidiMixerPlayer | null = null
  let stemPlayer: StemMixerPlayer | null = null
  let mixerTracks: (MixerTrack | StemTrack)[] = []
  let playerMode: 'stems' | 'midi' | null = null
  let midiLoading = false
  let midiPlayerError = ''
  let playbackState: 'stopped' | 'playing' | 'paused' = 'stopped'
  let playbackPosition = 0
  let playbackDuration = 0
  let playbackFrame: number | null = null
  let globalVolume = 0.5
  let trackLevels: Record<string, number> = {}
  let gaugeMultiplier = 6

  onMount(async () => {
    if (isPublicRoute && publicAccessKey) {
      await loadPublicMusic(publicAccessKey)
      return
    }

    if (adminPassword) {
      await tryAdminSession(adminPassword)
    }
  })

  onDestroy(() => {
    stopPlaybackLoop()
    if (stemPlayer) {
      void stemPlayer.dispose()
      stemPlayer = null
    }
    if (midiPlayer) {
      void midiPlayer.dispose()
      midiPlayer = null
    }
  })

  async function tryAdminSession(password: string) {
    adminLoading = true
    adminError = ''

    try {
      await login(password)
      adminLoggedIn = true
      window.localStorage.setItem('admin-password', password)
      await refreshMusics(password)
    } catch (error) {
      adminLoggedIn = false
      adminError = error instanceof Error ? error.message : 'Unable to log in'
      window.localStorage.removeItem('admin-password')
    } finally {
      adminLoading = false
    }
  }

  async function handleLogin() {
    adminSuccess = ''
    await tryAdminSession(adminPassword)
  }

  async function refreshMusics(password = adminPassword) {
    musics = await listMusics(password)
    editPublicIds = Object.fromEntries(musics.map((music) => [music.id, music.public_id ?? '']))
  }

  async function handleUpload() {
    if (!selectedFile) {
      adminError = 'Choose an .mscz file first.'
      return
    }

    uploadBusy = true
    adminError = ''
    adminSuccess = ''

    try {
      await uploadMusic(adminPassword, {
        file: selectedFile,
        title: uploadTitle,
        publicId: uploadPublicId,
      })

      uploadTitle = ''
      uploadPublicId = ''
      selectedFile = null
      const input = document.getElementById('mscz-input') as HTMLInputElement | null
      if (input) {
        input.value = ''
      }

      await refreshMusics()
      adminSuccess = 'Upload completed.'
    } catch (error) {
      adminError = error instanceof Error ? error.message : 'Upload failed'
    } finally {
      uploadBusy = false
    }
  }

  async function handleSavePublicId(musicId: string) {
    savingIdFor = musicId
    adminError = ''
    adminSuccess = ''

    try {
      const updated = await updatePublicId(adminPassword, musicId, editPublicIds[musicId] ?? '')
      musics = musics.map((music) => (music.id === musicId ? updated : music))
      editPublicIds = { ...editPublicIds, [musicId]: updated.public_id ?? '' }
      adminSuccess = 'Public id updated.'
    } catch (error) {
      adminError = error instanceof Error ? error.message : 'Unable to update public id'
    } finally {
      savingIdFor = ''
    }
  }

  async function handleRetryRender(musicId: string) {
    retryingFor = musicId
    adminError = ''
    adminSuccess = ''

    try {
      const updated = await retryRender(adminPassword, musicId)
      musics = musics.map((music) => (music.id === musicId ? updated : music))
      adminSuccess = 'Render retried successfully.'
    } catch (error) {
      adminError = error instanceof Error ? error.message : 'Retry failed'
    } finally {
      retryingFor = ''
    }
  }

  async function copyLink(value: string) {
    await navigator.clipboard.writeText(value)
    adminSuccess = 'Link copied to clipboard.'
  }

  function logout() {
    adminLoggedIn = false
    adminPassword = ''
    musics = []
    editPublicIds = {}
    adminSuccess = ''
    adminError = ''
    window.localStorage.removeItem('admin-password')
  }

  async function loadPublicMusic(accessKey: string) {
    publicLoading = true
    publicError = ''

    try {
      const music = await fetchPublicMusic(accessKey)
      publicMusic = music
      // Clear publicLoading NOW so the music-card branch renders and
      // bind:this={scoreContainer} fires before we try to use it.
      publicLoading = false
      await tick()
      await resetMixers()

      const mixerTask =
        music.stems_status === 'ready'
          ? loadStemMixer(accessKey)
          : music.midi_download_url
            ? loadMidiMixer(music.midi_download_url)
            : Promise.resolve()

      let scoreTask: Promise<void> = Promise.resolve()
      if (music.musicxml_url && scoreContainer) {
        scoreLoading = true
        const sv = new ScoreViewer(scoreContainer)
        scoreViewer = sv
        scoreTask = sv
          .load(music.musicxml_url)
          .then(() => {
            scoreLoaded = true
          })
          .catch((err: unknown) => {
            console.error('[ScoreViewer] load failed:', err)
            scoreError = err instanceof Error ? `${err.message}\n${err.stack ?? ''}` : String(err)
          })
          .finally(() => {
            scoreLoading = false
          })
      }

      await Promise.all([mixerTask, scoreTask])
    } catch (error) {
      publicError = error instanceof Error ? error.message : 'Unable to load this score'
    } finally {
      publicLoading = false
    }
  }

  async function resetMixers() {
    stopPlaybackLoop()
    playbackState = 'stopped'
    playbackPosition = 0
    playbackDuration = 0
    globalVolume = 0.5
    mixerTracks = []
    playerMode = null
    midiLoading = false
    midiPlayerError = ''

    if (stemPlayer) {
      await stemPlayer.dispose()
      stemPlayer = null
    }
    if (midiPlayer) {
      await midiPlayer.dispose()
      midiPlayer = null
    }
    if (scoreViewer) {
      scoreViewer.dispose()
      scoreViewer = null
    }
    scoreLoading = false
    scoreLoaded = false
    scoreError = ''
  }

  async function loadStemMixer(accessKey: string) {
    midiLoading = true
    midiPlayerError = ''

    try {
      const stems = await fetchStems(accessKey)
      if (stems.length === 0) {
        // No stems available – fall back to MIDI if present
        if (publicMusic?.midi_download_url) {
          await loadMidiMixer(publicMusic.midi_download_url)
        }
        return
      }

      stemPlayer = new StemMixerPlayer()
      const loaded = await stemPlayer.loadStems(
        stems.map((s) => ({
          id: String(s.track_index),
          name: s.track_name,
          instrumentName: s.instrument_name,
          streamUrl: s.stream_url,
        })),
      )
      mixerTracks = loaded.tracks
      playbackDuration = loaded.duration
      playbackPosition = 0
      playbackState = 'stopped'
      playerMode = 'stems'
    } catch (error) {
      midiPlayerError =
        error instanceof Error ? error.message : 'Unable to prepare stem playback'
      // Fall back to MIDI
      if (publicMusic?.midi_download_url) {
        await loadMidiMixer(publicMusic.midi_download_url)
      }
    } finally {
      midiLoading = false
    }
  }

  async function loadMidiMixer(url: string) {
    midiLoading = true
    midiPlayerError = ''

    try {
      midiPlayer = new MidiMixerPlayer()
      const loaded = await midiPlayer.loadFromUrl(url)
      mixerTracks = loaded.tracks
      playbackDuration = loaded.duration
      playbackPosition = 0
      playbackState = 'stopped'
      playerMode = 'midi'
    } catch (error) {
      midiPlayerError = error instanceof Error ? error.message : 'Unable to prepare MIDI playback'
    } finally {
      midiLoading = false
    }
  }

  async function togglePlayback() {
    const player = stemPlayer ?? midiPlayer
    if (!player || playbackDuration <= 0) {
      return
    }

    try {
      if (playbackState === 'playing') {
        player.pause()
        playbackState = 'paused'
        playbackPosition = player.getCurrentTime()
        stopPlaybackLoop()
        return
      }

      if (playbackPosition >= playbackDuration - 0.01) {
        player.seek(0)
        playbackPosition = 0
      }

      await player.play()
      playbackState = 'playing'
      startPlaybackLoop()
    } catch (error) {
      midiPlayerError = error instanceof Error ? error.message : 'Unable to start playback'
    }
  }

  function stopPlayback() {
    const player = stemPlayer ?? midiPlayer
    if (!player) {
      return
    }

    player.stop()
    playbackState = 'stopped'
    playbackPosition = 0
    stopPlaybackLoop()
  }

  function handleSeek(event: Event) {
    const player = stemPlayer ?? midiPlayer
    if (!player) {
      return
    }

    const target = event.currentTarget as HTMLInputElement
    const seconds = Number(target.value)
    player.seek(seconds)
    playbackPosition = seconds
  }

  function updateTrackVolume(trackId: string, event: Event) {
    const target = event.currentTarget as HTMLInputElement
    const volume = Number(target.value) / 100
    mixerTracks = mixerTracks.map((track) => (track.id === trackId ? { ...track, volume } : track))
    if (stemPlayer && playerMode === 'stems') {
      stemPlayer.setTrackVolume(trackId, volume)
    } else if (midiPlayer && playerMode === 'midi') {
      midiPlayer.setTrackVolume(trackId, volume)
    }
  }

  function updateGlobalVolume(event: Event) {
    const target = event.currentTarget as HTMLInputElement
    globalVolume = Number(target.value) / 100
    // Move every individual track slider to the new value
    mixerTracks = mixerTracks.map((track) => ({ ...track, volume: globalVolume }))
    if (stemPlayer && playerMode === 'stems') {
      for (const track of mixerTracks) {
        stemPlayer.setTrackVolume(track.id, globalVolume)
      }
    } else if (midiPlayer && playerMode === 'midi') {
      for (const track of mixerTracks) {
        midiPlayer.setTrackVolume(track.id, globalVolume)
      }
    }
  }

  function updateGaugeMultiplier(event: Event) {
    const target = event.currentTarget as HTMLInputElement
    gaugeMultiplier = Number(target.value)
    stemPlayer?.setLevelMultiplier(gaugeMultiplier)
  }

  function toggleTrackMute(trackId: string) {
    mixerTracks = mixerTracks.map((track) => {
      if (track.id !== trackId) {
        return track
      }

      const muted = !track.muted
      if (stemPlayer && playerMode === 'stems') {
        stemPlayer.setTrackMuted(trackId, muted)
      } else if (midiPlayer && playerMode === 'midi') {
        midiPlayer.setTrackMuted(trackId, muted)
      }
      return { ...track, muted }
    })
  }

  function startPlaybackLoop() {
    stopPlaybackLoop()

    const tick = () => {
      const player = stemPlayer ?? midiPlayer
      if (!player) {
        return
      }

      playbackPosition = player.getCurrentTime()
      scoreViewer?.seek(playbackPosition)

      if (stemPlayer && playerMode === 'stems') {
        const levels: Record<string, number> = {}
        for (const track of mixerTracks) {
          levels[track.id] = stemPlayer.getLevel(track.id)
        }
        trackLevels = levels
      }

      if (playbackState === 'playing') {
        if (playbackDuration > 0 && playbackPosition >= playbackDuration - 0.03) {
          player.pause()
          player.seek(playbackDuration)
          playbackState = 'paused'
          playbackPosition = playbackDuration
          stopPlaybackLoop()
          return
        }

        playbackFrame = requestAnimationFrame(tick)
      }
    }

    playbackFrame = requestAnimationFrame(tick)
  }

  function stopPlaybackLoop() {
    if (playbackFrame !== null) {
      cancelAnimationFrame(playbackFrame)
      playbackFrame = null
    }
    trackLevels = {}
  }

  function prettyDate(value: string) {
    return new Intl.DateTimeFormat(undefined, {
      dateStyle: 'medium',
      timeStyle: 'short',
    }).format(new Date(value))
  }

  function formatBytes(bytes: number) {
    if (bytes === 0) return '—'
    if (bytes < 1024) return `${bytes} B`
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`
    return `${(bytes / (1024 * 1024)).toFixed(1)} MB`
  }

  function formatTime(seconds: number) {
    const safeSeconds = Math.max(0, Math.floor(seconds))
    const minutes = Math.floor(safeSeconds / 60)
    const remainingSeconds = safeSeconds % 60
    return `${minutes}:${remainingSeconds.toString().padStart(2, '0')}`
  }

  function percentVolume(volume: number) {
    return Math.round(volume * 100)
  }
</script>

{#if isPublicRoute}
  <main class="page public-shell">
    <section class="hero-panel">
      <p class="eyebrow">MuseScore Reader</p>
      <h1>Listen to a shared score</h1>
      <p class="lede">
        This page plays high-quality per-instrument audio stems rendered by the backend with
        VSCO-2 Community Edition soundfonts. Each instrument gets its own volume control.
      </p>
    </section>

    <section class="content-panel">
      {#if publicLoading}
        <p class="status">Loading score...</p>
      {:else if publicError}
        <p class="status error">{publicError}</p>
      {:else if publicMusic}
        <div class="music-card public-card">
          <div>
            <p class="meta-label">Title</p>
            <h2>{publicMusic.title}</h2>
          </div>
          <div class="meta-grid">
            <div>
              <p class="meta-label">Filename</p>
              <p>{publicMusic.filename}</p>
            </div>
            <div>
              <p class="meta-label">Uploaded</p>
              <p>{prettyDate(publicMusic.created_at)}</p>
            </div>
            <div>
              <p class="meta-label">Stems</p>
              <p class="status-pill">{publicMusic.stems_status}</p>
            </div>
            <div>
              <p class="meta-label">Instruments</p>
              <p>{mixerTracks.length || 0}</p>
            </div>
          </div>

          <!-- Score viewer — OSMD renders into this container.
               The div is always present so bind:this is populated as soon
               as publicMusic is set. CSS hides it while empty. -->
          <div class="score-container" class:loaded={scoreLoaded} bind:this={scoreContainer}></div>
          {#if scoreLoading}
            <p class="status">Loading score…</p>
          {:else if scoreError}
            <p class="status error">Score: {scoreError}</p>
          {/if}

          {#if midiLoading}
            <p class="status">Preparing playback...</p>
          {:else if mixerTracks.length > 0}
            <div class="mixer-panel">
              <div class="transport-bar">
                <div class="transport-buttons">
                  <button class="button" on:click={togglePlayback}>
                    {playbackState === 'playing' ? 'Pause' : playbackPosition > 0 ? 'Resume' : 'Play'}
                  </button>
                  <button class="button ghost" on:click={stopPlayback}>Stop</button>
                </div>

                <div class="timeline-panel">
                  <input
                    class="timeline-slider"
                    type="range"
                    min="0"
                    max={playbackDuration || 0}
                    step="0.01"
                    value={playbackPosition}
                    on:input={handleSeek}
                  />
                  <p class="subtle">
                    {formatTime(playbackPosition)} / {formatTime(playbackDuration)}
                  </p>
                </div>
              </div>

              <div class="mixer-board">
                <div class="channel-strip global-strip">
                  <span class="channel-level">{Math.round(globalVolume * 100)}%</span>
                  <div class="channel-fader">
                    <input
                      type="range"
                      min="0"
                      max="100"
                      value={Math.round(globalVolume * 100)}
                      on:input={updateGlobalVolume}
                    />
                  </div>
                  <p class="channel-name">All</p>
                </div>

                <div class="channel-strip global-strip">
                  <span class="channel-level">x{gaugeMultiplier}</span>
                  <div class="channel-fader">
                    <input
                      type="range"
                      min="1"
                      max="20"
                      step="1"
                      value={gaugeMultiplier}
                      on:input={updateGaugeMultiplier}
                    />
                  </div>
                  <p class="channel-name">Gain</p>
                </div>

                <div class="channel-divider"></div>

                {#each mixerTracks as track}
                  <div class="channel-strip" class:muted={track.muted}>
                    <span class="channel-level">{percentVolume(track.volume)}%</span>
                    <div class="channel-fader">
                      <div class="channel-gauge">
                        <div
                          class="channel-gauge-fill"
                          style="height: {Math.round((trackLevels[track.id] ?? 0) * 100)}%"
                        ></div>
                      </div>
                      <input
                        type="range"
                        min="0"
                        max="100"
                        value={percentVolume(track.volume)}
                        on:input={(event) => updateTrackVolume(track.id, event)}
                      />
                    </div>
                    <button class="mute-btn" class:active={track.muted} on:click={() => toggleTrackMute(track.id)}>M</button>
                    <p class="channel-name">{track.name}</p>
                  </div>
                {/each}
              </div>
            </div>
          {:else if publicMusic.can_stream_audio && publicMusic.audio_stream_url}
            <div class="fallback-player">
              <p class="hint">
                Per-instrument stems are not available; playing the mixed audio preview instead.
                Individual volume control is not possible in this mode.
              </p>
              <audio class="player" controls preload="metadata" src={publicMusic.audio_stream_url}>
                Your browser does not support audio playback.
              </audio>
            </div>
          {:else}
            <p class="hint">
              Playback is not available yet.
              {#if publicMusic.stems_error}
                <br />
                <span>Stems: {publicMusic.stems_error}</span>
              {/if}
              {#if publicMusic.midi_error}
                <br />
                <span>MIDI: {publicMusic.midi_error}</span>
              {/if}
            </p>
          {/if}

          {#if midiPlayerError}
            <p class="status error">{midiPlayerError}</p>
          {/if}

          {#if publicMusic.audio_error && !mixerTracks.length}
            <p class="hint">{publicMusic.audio_error}</p>
          {/if}

          <div class="actions">
            <a class="button secondary" href={publicMusic.download_url}>Download .mscz</a>
            {#if publicMusic.audio_stream_url}
              <a class="button secondary" href={publicMusic.audio_stream_url} download="preview.mp3">Download audio</a>
            {/if}
            {#if publicMusic.midi_download_url}
              <a class="button secondary" href={publicMusic.midi_download_url}>Download MIDI</a>
            {/if}
          </div>
        </div>
      {/if}
    </section>
  </main>
{:else}
  <main class="page admin-shell">
    <section class="hero-panel">
      <p class="eyebrow">MuseScore Reader</p>
      <h1>Private upload desk</h1>
      <p class="lede">
        Upload `.mscz` scores, store them in the configured backend, and hand out either a random
        share link or a friendly public id.
      </p>
    </section>

    <section class="content-panel">
      {#if !adminLoggedIn}
        <div class="music-card auth-card">
          <label class="field">
            <span>Admin password</span>
            <input
              bind:value={adminPassword}
              type="password"
              placeholder="Hard-coded backend password"
              on:keydown={(event) => event.key === 'Enter' && handleLogin()}
            />
          </label>
          <button class="button" disabled={adminLoading} on:click={handleLogin}>
            {adminLoading ? 'Checking...' : 'Open admin'}
          </button>
          {#if adminError}
            <p class="status error">{adminError}</p>
          {/if}
        </div>
      {:else}
        <div class="toolbar">
          <div>
            <p class="meta-label">Session</p>
            <p class="toolbar-title">Authenticated with the hard-coded admin password</p>
          </div>
          <button class="button ghost" on:click={logout}>Log out</button>
        </div>

        <div class="music-card upload-card">
          <div class="card-header">
            <div>
              <p class="meta-label">Upload</p>
              <h2>Add a MuseScore score</h2>
            </div>
          </div>

          <div class="upload-grid">
            <label class="field">
              <span>Title</span>
              <input bind:value={uploadTitle} placeholder="Optional display title" />
            </label>
            <label class="field">
              <span>Public id</span>
              <input bind:value={uploadPublicId} placeholder="Optional friendly id" />
            </label>
            <label class="field file-field">
              <span>MSCZ file</span>
              <input
                id="mscz-input"
                type="file"
                accept=".mscz"
                on:change={(event) => {
                  const target = event.currentTarget as HTMLInputElement
                  selectedFile = target.files?.[0] ?? null
                }}
              />
            </label>
          </div>

          <button class="button" disabled={uploadBusy} on:click={handleUpload}>
            {uploadBusy ? 'Uploading...' : 'Upload score'}
          </button>
        </div>

        {#if adminError}
          <p class="status error">{adminError}</p>
        {/if}

        {#if adminSuccess}
          <p class="status success">{adminSuccess}</p>
        {/if}

        <section class="list-section">
          <div class="card-header">
            <div>
              <p class="meta-label">Library</p>
              <h2>Uploaded scores</h2>
            </div>
          </div>

          {#if musics.length === 0}
            <div class="music-card">
              <p class="hint">No uploads yet.</p>
            </div>
          {:else}
            <div class="music-list">
              {#each musics as music}
                <article class="music-card">
                  <div class="music-topline">
                    <div>
                      <h3>{music.title}</h3>
                      <p class="subtle">{music.filename}</p>
                    </div>
                    <p class="status-pill">{music.midi_status} midi</p>
                  </div>

                  <div class="meta-grid">
                    <div>
                      <p class="meta-label">Random link</p>
                      <a href={music.public_url} target="_blank" rel="noreferrer">{music.public_url}</a>
                    </div>
                    <div>
                      <p class="meta-label">Uploaded</p>
                      <p>{prettyDate(music.created_at)}</p>
                    </div>
                    <div>
                      <p class="meta-label">Audio export</p>
                      <p>{music.audio_status}</p>
                    </div>
                    <div>
                      <p class="meta-label">Stems</p>
                      <p>{music.stems_status}</p>
                    </div>
                    <div>
                      <p class="meta-label">Stems size</p>
                      <p>{formatBytes(music.stems_total_bytes)}</p>
                    </div>
                  </div>

                  {#if music.audio_error}
                    <p class="hint">{music.audio_error}</p>
                  {/if}

                  {#if music.stems_error}
                    <p class="hint">{music.stems_error}</p>
                  {/if}

                  <div class="id-row">
                    <label class="field">
                      <span>Friendly public id</span>
                      <input bind:value={editPublicIds[music.id]} placeholder="example: moonlight-sonata" />
                    </label>
                    <button
                      class="button secondary"
                      disabled={savingIdFor === music.id}
                      on:click={() => handleSavePublicId(music.id)}
                    >
                      {savingIdFor === music.id ? 'Saving...' : 'Save id'}
                    </button>
                  </div>

                  <div class="actions">
                    <button class="button ghost" on:click={() => copyLink(music.public_url)}>
                      Copy random link
                    </button>
                    {#if music.public_id_url}
                      <button class="button ghost" on:click={() => copyLink(music.public_id_url!)}>
                        Copy id link
                      </button>
                    {/if}
                    {#if music.stems_status !== 'ready'}
                      <button
                        class="button secondary"
                        disabled={retryingFor === music.id}
                        on:click={() => handleRetryRender(music.id)}
                      >
                        {retryingFor === music.id ? 'Retrying...' : 'Retry render'}
                      </button>
                    {/if}
                    <a class="button secondary" href={music.download_url} target="_blank" rel="noreferrer">
                      Original file
                    </a>
                    {#if music.midi_download_url}
                      <a class="button secondary" href={music.midi_download_url} target="_blank" rel="noreferrer">
                        MIDI export
                      </a>
                    {/if}
                    {#if music.public_id_url}
                      <a class="button secondary" href={music.public_id_url} target="_blank" rel="noreferrer">
                        Open id link
                      </a>
                    {/if}
                  </div>
                </article>
              {/each}
            </div>
          {/if}
        </section>
      {/if}
    </section>
  </main>
{/if}
