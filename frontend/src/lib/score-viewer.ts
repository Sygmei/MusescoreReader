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

    this.osmd = new OpenSheetMusicDisplay(this.container, {
      autoResize: false,
      backend: 'svg',
      drawTitle: false,
      drawSubtitle: false,
      drawComposer: false,
      drawCursors: true,
      followCursor: false,
    })

    // zoom must be set as a property before render(), not as a constructor
    // option — OSMD 1.9 ignores the constructor value in most code paths.
    this.osmd.zoom = 0.35

    await this.osmd.load(xmlText)

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
    const svgTopPx = svgRect.top
    const svgH     = svgRect.height
    console.log(`[ScoreViewer] buildTimeMap: svgH=${svgH}, containerH=${this.container.getBoundingClientRect().height}`)

    const measureBpm = this.buildMeasureBpmMap()
    const map: TimeEntry[] = []
    const geoms: Array<{ top: number; height: number }> = []

    cursor.show()
    cursor.reset()

    // Use the container as the position reference, not the SVG element.
    // OSMD renders the cursor as an absolutely-positioned <div> INSIDE the
    // container (not inside the SVG).  getBoundingClientRect() gives viewport
    // coordinates; subtracting the container's viewport top gives the position
    // within the container's scrollable content area — which is exactly what
    // we need for scrollTop comparisons later.
    const containerRect = this.container.getBoundingClientRect()
    let step = 0
    while (!cursor.iterator.EndReached) {
      const wholeNotes: number = cursor.iterator.currentTimeStamp?.RealValue ?? 0
      const measureIdx: number = cursor.iterator.CurrentMeasureIndex ?? 0
      const bpm = measureBpm.get(measureIdx) ?? measureBpm.get(0) ?? 120
      map.push({ seconds: (wholeNotes * 240) / bpm, step })

      try {
        const el = cursor.cursorElement as HTMLElement | null
        if (el) {
          const r = el.getBoundingClientRect()
          // Position relative to container's content area top.
          const top = r.top - containerRect.top + this.container.scrollTop
          geoms.push({ top, height: r.height })
        } else {
          geoms.push({ top: -1, height: 0 })
        }
      } catch {
        geoms.push({ top: -1, height: 0 })
      }

      cursor.next()
      step++
    }

    this.timeMap = map
    this.currentStep = 0
    cursor.reset()
    cursor.show()

    console.log(`[ScoreViewer] walked ${geoms.length} steps; svgH=${svgH}px; first5 geoms:`, geoms.slice(0, 5))
    this.buildSystemMapFromGeoms(geoms, svgH)
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

    // Resize the container to this system's exact page height so no adjacent
    // system bleeds into view.  +16 compensates for the 8px top+bottom padding
    // that .score-container.loaded adds (box-sizing:border-box means padding
    // is subtracted from the height, so we add it back).
    const h = this.systemMap[i].heightPx
    if (h > 0) this.container.style.height = `${h + 16}px`

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
      cursor.reset()
      this.currentStep = 0
      this.currentSystemIdx = -1
    }

    while (this.currentStep < targetStep && !cursor.iterator.EndReached) {
      cursor.next()
      this.currentStep++
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

  reset(): void {
    this.osmd?.cursor?.reset()
    this.currentStep = 0
    this.currentSystemIdx = -1
    if (this.systemMap.length > 0) {
      this.container.scrollTop = 0
    }
  }

  dispose(): void {
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

  const forPartCount = (xmlText.match(/<for-part[^>]*>/g) ?? []).length
  const transposeCount = (xmlText.match(/<transpose[^>]*>/g) ?? []).length
  const emptyStepCount = (xmlText.match(/<display-step><\/display-step>/g) ?? []).length
  console.log(`[ScoreViewer] stripped ${forPartCount} <for-part>, ${transposeCount} <transpose>, ${emptyStepCount} empty <display-step>`)

  return result
}
