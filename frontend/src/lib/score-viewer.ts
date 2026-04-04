/**
 * ScoreViewer — wraps OpenSheetMusicDisplay (OSMD) with a paged display:
 *
 *  - The score renders normally (measures wrap into rows / systems).
 *  - A "system map" is built after render: the Y offset of each system row.
 *  - The container is sized to show exactly ONE system at a time (height =
 *    tallest system + padding).  overflow is hidden so nothing bleeds.
 *  - On every seek() call we detect which system the cursor is on and
 *    instantly snap-scroll (scrollTop) so that system fills the window.
 *    No smooth scroll — the intent is a clean page-flip, not continuous scroll.
 *  - Instrument names are part of OSMD's normal system rendering and are
 *    always visible on the left edge of each system.
 */

// eslint-disable-next-line @typescript-eslint/no-explicit-any
type OSMD = any

interface TimeEntry {
  seconds: number
  step: number
  /** Cursor element left edge relative to SVG left; -1 if not measured. */
  xPx: number
  /** Cursor element top relative to container content area; -1 if not measured. */
  topPx: number
}

/** Y pixel offset (relative to SVG top) for each rendered system. */
interface SystemEntry {
  topPx: number      // scrollTop that puts this system at the top of the viewport
  heightPx: number   // height of this system row
}

export class ScoreViewer {
  private osmd: OSMD = null
  private timeMap: TimeEntry[] = []
  private systemMap: SystemEntry[] = []
  private currentStep = 0
  private currentSystemIdx = -1
  private container: HTMLElement
  private _clickHandler: ((e: MouseEvent) => void) | null = null

  /** Called when the user clicks on the score. Argument is seconds into the piece. */
  onClickSeek: ((seconds: number) => void) | null = null

  constructor(container: HTMLElement) {
    this.container = container
  }

  async load(musicXmlUrl: string): Promise<void> {
    const response = await fetch(musicXmlUrl)
    if (!response.ok) {
      throw new Error(`Failed to fetch MusicXML: HTTP ${response.status}`)
    }
    let xmlText = await response.text()
    xmlText = stripUnsupportedElements(xmlText)

    const { OpenSheetMusicDisplay } = await import('opensheetmusicdisplay')

    this.osmd = new OpenSheetMusicDisplay(
      this.container,
      {
        autoResize: false,
        backend: 'svg',
        // Keep explicit rest measures in the rendered model so the cursor/seek
        // map can advance through sections where upper parts rest while lower
        // parts continue playing.
        autoGenerateMultipleRestMeasuresFromRestMeasures: false,
        drawTitle: false,
        drawSubtitle: false,
        drawComposer: false,
        drawCursors: true,
        followCursor: false,
        zoom: 0.35,
      } as Record<string, unknown>,
    )

    await this.osmd.load(xmlText)

    // Ensure abbreviation labels = full names both in the XML (already done
    // in stripUnsupportedElements) AND at the OSMD model level.
    //
    // OSMD caches abbreviation text from the XML during load() and uses those
    // cached strings to compute the label-column width before render() runs.
    // Overriding the private abbreviationStr field here forces the render to
    // use the full name width for the label column on every system.
    const parts: unknown[] = (this.osmd as any).Sheet?.Parts ?? []
    for (const part of parts) {
      try {
        const fullName: string = (part as any).nameStr ?? (part as any).Name ?? ''
        if (fullName) (part as any).abbreviationStr = fullName
      } catch { /* ignore */ }
    }
    this.osmd.zoom = 0.35
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    if (typeof (this.osmd as any).setOptions === 'function') {
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      ;(this.osmd as any).setOptions({ zoom: 0.35 })
    }
    console.log('[ScoreViewer] zoom set to', this.osmd.zoom)

    // Ensure part names AND abbreviations are both rendered.
    // We rewrote the abbreviations in the XML to equal the full names, so all
    // systems will show the same label text.  Set EngravingRules explicitly so
    // OSMD doesn't fall back to its defaults and suppress either.
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const rules = (this.osmd as any).EngravingRules
    if (rules) {
      rules.RenderPartNames = true
      rules.RenderPartAbbreviations = true
    }

    // The CSS base rule has height:0; overflow:hidden on this container.
    // We must override BOTH before calling osmd.render() for two reasons:
    //  1. OSMD reads container.offsetWidth to determine page width — that
    //     works fine because width is never constrained.
    //  2. After render, we call getBoundingClientRect() on SVG child elements
    //     to detect system row positions.  With height:0 + overflow:hidden
    //     the browser clips everything to 0px, making all measurements 0.
    //     Setting height:auto allows the container to expand to the SVG's
    //     intrinsic height so measurements return real pixel values.
    this.container.style.height = 'auto'
    this.container.style.overflow = 'visible'

    await this.osmd.render()
    // One rAF so the browser has committed layout before we measure.
    await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()))

    try {
      const cursor = this.osmd?.cursor
      if (!cursor) {
        return
      }

      // OSMD may otherwise skip hidden/rest-derived positions while walking the
      // cursor, which truncates our time map on scores whose upper staves go
      // silent before lower staves do.
      cursor.SkipInvisibleNotes = false

      // buildTimeMap also builds systemMap from cursor element pixel positions.
      // height:auto + overflow:visible (set above) must be in effect here so
      // cursor.cursorElement.getBoundingClientRect() returns real values.
      this.buildTimeMap()

      // Constrain the container to one system row.  height and scrollTop are
      // managed per-flip inside snapToSystem(), which sizes the container to
      // the current system's exact page height.  We just need overflow:hidden
      // here so the full SVG doesn't show before the first snap.
      this.container.style.overflow = 'hidden'

      this.osmd.cursor.show()
      this.snapToSystem(0)
      this.attachClickHandler()
    } catch (cursorErr) {
      console.warn('[ScoreViewer] cursor init failed (non-fatal):', cursorErr)
      this.container.style.overflow = 'hidden'
    }
  }

  /**
   * Derive system row positions from the cursor element's pixel coordinates.
   *
   * We walk the cursor without calling cursor.hide() because hide() sets
   * display:none, which makes getBoundingClientRect() return all zeros.
   * The container still has CSS visibility:hidden (class .score-container,
   * not yet .loaded) so there is no visible flicker during the walk.
   *
   * The OSMD cursor element spans the full height of the system it sits in
   * (top staff to bottom staff, across all instruments).  Clustering the
   * per-step top-Y values therefore gives us exact system top + height.
   */
  private buildTimeMap(): void {
    const cursor = this.osmd?.cursor
    if (!cursor) return

    const svgEl = this.container.querySelector('svg')
    if (!svgEl) return
    const svgRect = svgEl.getBoundingClientRect()
    const svgH     = svgRect.height
    console.log(`[ScoreViewer] buildTimeMap: svgH=${svgH}, containerH=${this.container.getBoundingClientRect().height}`)

    const measureBpm = this.buildMeasureBpmMap()
    const map: TimeEntry[] = []
    const geoms: Array<{ top: number; height: number }> = []

    this.resetCursorIterator(cursor)
    const iterator = cursor.iterator
    if (!iterator) return

    const containerRect = this.container.getBoundingClientRect()
    let step = 0
    // Accumulate seconds via deltas rather than using RealValue directly.
    // RealValue is the musical timestamp from the start of the score; on a
    // repeat jump it resets to the earlier value, making the timeline
    // non-monotonic.  Accumulating deltas keeps the timeline strictly
    // increasing across repeats (one note = one forward step in time).
    let cumulativeSeconds = 0

    while (!iterator.EndReached) {
      cursor.update()

      const enrolledValue = this.getIteratorTimestamp(iterator)
      const measureIdx: number = iterator.CurrentMeasureIndex ?? 0
      const bpm = measureBpm.get(measureIdx) ?? measureBpm.get(0) ?? 120

      // Duration of this beat in whole notes, needed when a repeat jump
      // means nextRealValue ≤ realValue so we cannot use the delta.
      const beatWholeNotes = this.getBeatDuration(cursor)

      map.push({ seconds: cumulativeSeconds, step, xPx: -1, topPx: -1 })

      try {
        const el = cursor.cursorElement as HTMLElement | null
        if (el) {
          const r = el.getBoundingClientRect()
          const top = r.top - containerRect.top + this.container.scrollTop
          const left = r.left - svgRect.left
          geoms.push({ top, height: r.height })
          map[map.length - 1].xPx = left
          map[map.length - 1].topPx = top
        } else {
          geoms.push({ top: -1, height: 0 })
        }
      } catch {
        geoms.push({ top: -1, height: 0 })
      }

      iterator.moveToNext()
      step++

      if (!iterator.EndReached) {
        const nextEnrolledValue = this.getIteratorTimestamp(iterator)
        const delta = nextEnrolledValue - enrolledValue
        if (delta > 0) {
          // Normal forward step: use the actual inter-step delta for accuracy.
          cumulativeSeconds += (delta * 240) / bpm
        } else {
          // Repeat jump (or D.C./D.S.): nextRealValue went backward or stayed
          // the same.  Use the note's own duration so the timeline advances
          // by exactly one beat.
          cumulativeSeconds += (beatWholeNotes * 240) / bpm
        }
      }
    }

    this.timeMap = map
    this.currentStep = 0
    this.resetCursorIterator(cursor)

    console.log(`[ScoreViewer] walked ${geoms.length} steps (incl. repeats); svgH=${svgH}px`)
    this.buildSystemMapFromGeoms(geoms, svgH)
  }

  private getIteratorTimestamp(iterator: any): number {
    return (
      iterator?.CurrentEnrolledTimestamp?.RealValue ??
      iterator?.CurrentSourceTimestamp?.RealValue ??
      iterator?.currentTimeStamp?.RealValue ??
      0
    )
  }

  private resetCursorIterator(cursor: OSMD): void {
    const manager = this.osmd?.Sheet?.MusicPartManager
    const iterator = manager?.getIterator?.()
    if (!iterator) return

    iterator.SkipInvisibleNotes = false
    cursor.iterator = iterator
    cursor.show()
    cursor.update()
    this.currentStep = 0
    this.currentSystemIdx = -1
  }

  /**
   * Return the duration in whole notes of the beat currently under the cursor,
   * derived from the shortest note in any voice entry at this position.
   * Falls back to a quarter note (0.25) if nothing else is available.
   */
  private getBeatDuration(cursor: OSMD): number {
    try {
      const entries: unknown[] = cursor.VoicesUnderCursor?.() ?? []
      let min = Infinity
      for (const entry of entries as Array<{ Notes?: Array<{ Length?: { RealValue?: number } }> }>) {
        for (const note of entry.Notes ?? []) {
          const len = note.Length?.RealValue ?? 0
          if (len > 0 && len < min) min = len
        }
      }
      return Number.isFinite(min) ? min : 0.25
    } catch {
      return 0.25
    }
  }

  private buildSystemMapFromGeoms(
    geoms: Array<{ top: number; height: number }>,
    svgH: number
  ): void {
    this.systemMap = []
    const valid = geoms.filter((g) => g.top >= 0 && g.height > 0)

    if (valid.length === 0) {
      if (svgH > 0) this.systemMap = [{ topPx: 0, heightPx: svgH }]
      console.warn('[ScoreViewer] no cursor geometry — single system fallback')
      return
    }

    // Cluster steps by cursor top-Y.
    type Cluster = { top: number; bottom: number }
    const clusters: Cluster[] = []
    for (const g of valid) {
      const last = clusters[clusters.length - 1]
      if (!last || g.top - last.top > 30) {
        clusters.push({ top: g.top, bottom: g.top + g.height })
      } else {
        last.bottom = Math.max(last.bottom, g.top + g.height)
      }
    }

    // MARGIN: px above the cursor top to include when scrolling to a system.
    // Needed because clef symbols, time signatures, and top staff lines render
    // slightly above where the cursor element starts.
    const MARGIN = 16

    this.systemMap = clusters.map((c, i) => {
      // System 0: always show from y=0 so OSMD's top margin and first stave
      // lines are never clipped.
      // System N>0: scroll to MARGIN px above the cursor.
      const topPx = i === 0 ? 0 : Math.max(0, c.top - MARGIN)

      // Page height = distance from THIS system's topPx to the NEXT system's
      // topPx.  This guarantees the viewport stops exactly where the next page
      // starts — no bleed, regardless of system height variation.
      const nextTopPx = i < clusters.length - 1
        ? Math.max(0, clusters[i + 1].top - MARGIN)
        : svgH
      const heightPx = nextTopPx - topPx

      return { topPx, heightPx }
    })
    console.log('[ScoreViewer] systemMap:', this.systemMap.map((s, i) =>
      `[${i}] top=${s.topPx} h=${s.heightPx}`))
  }

  private snapToSystem(idx: number): void {
    if (this.systemMap.length === 0) return
    const i = Math.max(0, Math.min(idx, this.systemMap.length - 1))
    if (i === this.currentSystemIdx) return
    this.currentSystemIdx = i

    // Resize the container to this system's exact page height.
    // No padding offset needed — .score-container.loaded has no padding,
    // so border-box height == visible viewport (minus 2px for 1px top+bottom
    // border, which is imperceptible).
    const h = this.systemMap[i].heightPx
    if (h > 0) this.container.style.height = `${h}px`

    // Instant snap — no smooth-scroll so it feels like a page flip.
    this.container.scrollTop = this.systemMap[i].topPx
  }



  private buildMeasureBpmMap(): Map<number, number> {
    const result = new Map<number, number>()
    result.set(0, 120)
    try {
      const measures = this.osmd.Sheet?.SourceMeasures as Array<{
        TempoExpressions?: Array<{ InstantaneousTempo?: { TempoInBpm?: number } }>
      }> | undefined
      if (!measures) return result
      let lastBpm = 120
      for (let i = 0; i < measures.length; i++) {
        for (const expr of measures[i].TempoExpressions ?? []) {
          const t = expr.InstantaneousTempo?.TempoInBpm
          if (typeof t === 'number' && t > 0) { lastBpm = t; break }
        }
        result.set(i, lastBpm)
      }
    } catch { /* ignore */ }
    return result
  }

  seek(seconds: number): void {
    const cursor = this.osmd?.cursor
    if (!cursor || this.timeMap.length === 0) return

    // Binary search: highest entry with .seconds <= seconds
    let lo = 0, hi = this.timeMap.length - 1
    while (lo < hi) {
      const mid = (lo + hi + 1) >> 1
      if (this.timeMap[mid].seconds <= seconds) lo = mid; else hi = mid - 1
    }

    const targetStep = this.timeMap[lo].step
    if (targetStep === this.currentStep) return

    if (targetStep < this.currentStep) {
      this.resetCursorIterator(cursor)
    }

    const iterator = cursor.iterator
    if (!iterator) return

    while (this.currentStep < targetStep && !iterator.EndReached) {
      iterator.moveToNext()
      this.currentStep++
      cursor.update()
    }

    this.flipPageIfNeeded(cursor)
  }

  /**
   * Determine which system row the cursor is currently on by comparing the
   * cursor element's Y position against the system map, then snap to it.
   */
  private flipPageIfNeeded(cursor: OSMD): void {
    try {
      const el: HTMLElement | null = cursor.cursorElement
      if (!el || this.systemMap.length === 0) return

      const svgEl = this.container.querySelector('svg')
      if (!svgEl) return

      const svgRect = svgEl.getBoundingClientRect()
      const cursorRect = el.getBoundingClientRect()
      // Y of the cursor relative to the SVG top (same coordinate space as systemMap).
      const cursorY = cursorRect.top - svgRect.top

      // Find the system whose top is closest to (and not below) the cursor.
      let bestIdx = 0
      for (let i = 0; i < this.systemMap.length; i++) {
        if (this.systemMap[i].topPx <= cursorY + 4) bestIdx = i
      }

      this.snapToSystem(bestIdx)
    } catch { /* ignore */ }
  }

  /**
   * Attach a click listener on the score SVG.  A click is mapped to the
   * nearest time-map entry by comparing the click's X coordinate (as a
   * fraction of SVG width) to each step's cursor X position.
   *
   * Because the cursor element is a <div> overlay on top of the SVG, we
   * listen on the container for both; the SVG and the cursor <div> both
   * bubble up to it.  We convert the viewport-Y of the click back into
   * SVG coordinates to find which system was clicked, then pick the
   * time-map entry whose cursor Y is closest to that system's top, and
   * whose cursor X is closest to the click X.
   */
  private attachClickHandler(): void {
    if (this._clickHandler) {
      this.container.removeEventListener('click', this._clickHandler)
    }

    this._clickHandler = (e: MouseEvent) => {
      if (this.timeMap.length === 0 || !this.osmd?.cursor) return

      const svgEl = this.container.querySelector('svg')
      if (!svgEl) return

      const svgRect = svgEl.getBoundingClientRect()
      // Y relative to SVG top = same coordinate space as systemMap.topPx and entry.topPx.
      // svgRect.top already accounts for scroll: when container.scrollTop = S, the SVG
      // is shifted S px upward in the viewport, so svgRect.top = containerTop − S.
      // Therefore (e.clientY − svgRect.top) = (e.clientY − containerTop + S), which
      // is exactly the offset from the SVG's top edge — no need to add scrollTop again.
      const clickYInSvg = e.clientY - svgRect.top
      // X relative to SVG left (same coordinate space as entry.xPx).
      const clickXInSvg = e.clientX - svgRect.left

      // Find which system was clicked.
      let clickedSystem = 0
      for (let i = 0; i < this.systemMap.length; i++) {
        if (this.systemMap[i].topPx <= clickYInSvg + 4) clickedSystem = i
      }

      // Filter to entries that belong to the clicked system.
      // An entry belongs to system[i] when its topPx falls inside that
      // system's vertical band [systemMap[i].topPx, systemMap[i+1].topPx).
      const inSystem = this.timeMap.filter((entry) => {
        if (entry.topPx < 0) return false
        let sysIdx = 0
        for (let i = 0; i < this.systemMap.length; i++) {
          if (this.systemMap[i].topPx <= entry.topPx + 4) sysIdx = i
        }
        return sysIdx === clickedSystem
      })

      const pool = inSystem.filter(e => e.xPx >= 0)
      if (pool.length === 0) return

      // The cursor xPx is the LEFT edge of each note.  The note occupies the
      // horizontal span from its own xPx up to the next note's xPx.  So the
      // right choice is always the LAST entry whose xPx is ≤ clickX — i.e.
      // the note that starts at or before the click position.
      // Fall back to the first entry if the click is to the left of every note.
      const sorted = [...pool].sort((a, b) => a.xPx - b.xPx)
      let best = sorted[0]
      for (const entry of sorted) {
        if (entry.xPx <= clickXInSvg) best = entry
        else break
      }

      this.onClickSeek?.(best.seconds)
    }

    this.container.addEventListener('click', this._clickHandler)
  }

  reset(): void {
    if (this.osmd?.cursor) {
      this.resetCursorIterator(this.osmd.cursor)
    }
    if (this.systemMap.length > 0) {
      this.container.scrollTop = 0
    }
  }

  dispose(): void {
    if (this._clickHandler) {
      this.container.removeEventListener('click', this._clickHandler)
      this._clickHandler = null
    }
    try { this.osmd?.cursor?.hide() } catch { /* ignore */ }
    this.osmd = null
    this.timeMap = []
    this.systemMap = []
    this.currentStep = 0
    this.container.innerHTML = ''
  }
}

/**
 * Remove MusicXML elements that OSMD 1.9.x doesn't support, using plain
 * string replacement on the original text.
 *
 * IMPORTANT: do NOT use DOMParser + XMLSerializer here — XMLSerializer
 * injects xmlns namespace attributes into every element, which corrupts
 * OSMD's internal XML lookups and causes note pitches to parse as undefined,
 * producing "NoteEnum[FundamentalNote] is undefined" crashes.
 *
 * Elements removed:
 *   <for-part>  — MusicXML 4.0 part-linking blocks written by MuseScore 4
 *                 for transposing instruments (Bb clarinet, F horn, etc.).
 *   <transpose> — older per-measure transposition hints.
 */
function stripUnsupportedElements(xmlText: string): string {
  // <for-part> — MusicXML 4.0 part-linking blocks (MuseScore 4 transposing instruments).
  // <transpose> — older per-measure transposition hints.
  // Both cause OSMD 1.9.x to crash with "NoteEnum[FundamentalNote] is undefined".
  let result = xmlText.replace(/<for-part[^>]*>[\s\S]*?<\/for-part>/g, '')
  result = result.replace(/<transpose[^>]*>[\s\S]*?<\/transpose>/g, '')

  // <display-step></display-step> — MuseScore 4 writes empty display-step inside
  // <unpitched> percussion notes (alongside bogus octave values like 1434).
  // OSMD tries NoteEnum[""] → undefined → .toLowerCase() → crash.
  // Replace with a valid placeholder note name so OSMD can at least parse them.
  result = result.replace(/<display-step><\/display-step>/g, '<display-step>B</display-step>')
  // Also clamp the absurd octave values MuseScore 4 writes for these same notes.
  result = result.replace(/<display-octave>(\d{3,})<\/display-octave>/g, '<display-octave>4</display-octave>')

  // Replace each <part-abbreviation> with the corresponding <part-name> so
  // OSMD renders the same full instrument name on every system row, not just
  // the first.  OSMD uses the abbreviation text for systems 2+; by making
  // abbreviations equal to full names, all pages look identical.
  // Also INSERT a <part-abbreviation> for parts that don't have one at all,
  // otherwise OSMD stores an empty string and computes a zero-width label
  // column for those parts on systems 2+.
  result = result.replace(
    /<score-part[^>]*>[\s\S]*?<\/score-part>/g,
    (scorePart) => {
      const nameMatch = scorePart.match(/<part-name[^>]*>([\s\S]*?)<\/part-name>/)
      if (!nameMatch) return scorePart
      const fullName = nameMatch[1]
      if (/<part-abbreviation/.test(scorePart)) {
        // Replace existing abbreviation with full name.
        return scorePart.replace(
          /<part-abbreviation[^>]*>[\s\S]*?<\/part-abbreviation>/g,
          `<part-abbreviation>${fullName}</part-abbreviation>`
        )
      } else {
        // No abbreviation element at all — insert one after </part-name>.
        return scorePart.replace(
          /(<\/part-name>)/,
          `$1<part-abbreviation>${fullName}</part-abbreviation>`
        )
      }
    }
  )

  const forPartCount = (xmlText.match(/<for-part[^>]*>/g) ?? []).length
  const transposeCount = (xmlText.match(/<transpose[^>]*>/g) ?? []).length
  const emptyStepCount = (xmlText.match(/<display-step><\/display-step>/g) ?? []).length
  console.log(`[ScoreViewer] stripped ${forPartCount} <for-part>, ${transposeCount} <transpose>, ${emptyStepCount} empty <display-step>`)

  return result
}
