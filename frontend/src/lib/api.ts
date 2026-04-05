export type AdminMusic = {
  id: string
  title: string
  filename: string
  content_type: string
  audio_status: string
  audio_error: string | null
  midi_status: string
  midi_error: string | null
  stems_status: string
  stems_error: string | null
  public_token: string
  public_id: string | null
  public_url: string
  public_id_url: string | null
  download_url: string
  midi_download_url: string | null
  quality_profile: StemQualityProfile
  created_at: string
  stems_total_bytes: number
}

export type StemQualityProfile = 'compact' | 'standard' | 'high'

export const STEM_QUALITY_PROFILES: Array<{
  value: StemQualityProfile
  label: string
  description: string
}> = [
  {
    value: 'compact',
    label: 'Compact',
    description: 'Smaller stem files with more aggressive Opus compression at 24k.',
  },
  {
    value: 'standard',
    label: 'Standard',
    description: 'Balanced stem quality and size at 32k.',
  },
  {
    value: 'high',
    label: 'High',
    description: 'Higher stem quality with larger files at 48k.',
  },
]

export type PublicMusic = {
  title: string
  filename: string
  audio_status: string
  audio_error: string | null
  can_stream_audio: boolean
  audio_stream_url: string | null
  midi_status: string
  midi_error: string | null
  midi_download_url: string | null
  stems_status: string
  stems_error: string | null
  musicxml_url: string | null
  download_url: string
  created_at: string
}

export type Stem = {
  track_index: number
  track_name: string
  instrument_name: string
  full_stem_url: string
  duration_seconds: number
  drum_map?: Array<{
    pitch: number
    name: string
    head?: string | null
    line?: number | null
    voice?: number | null
    stem?: number | null
    shortcut?: string | null
  }> | null
}

type JsonOptions = RequestInit & {
  password?: string
}

async function requestJson<T>(path: string, options: JsonOptions = {}): Promise<T> {
  const headers = new Headers(options.headers)

  if (options.password) {
    headers.set('x-admin-password', options.password)
  }

  if (!(options.body instanceof FormData) && !headers.has('content-type')) {
    headers.set('content-type', 'application/json')
  }

  const response = await fetch(path, {
    ...options,
    headers,
  })

  if (!response.ok) {
    let message = `Request failed with status ${response.status}`

    try {
      const payload = (await response.json()) as { error?: string }
      if (payload.error) {
        message = payload.error
      }
    } catch {
      // Ignore JSON parsing errors and keep the fallback message.
    }

    throw new Error(message)
  }

  return (await response.json()) as T
}

async function requestBlob(path: string, options: JsonOptions = {}): Promise<Blob> {
  const headers = new Headers(options.headers)

  if (options.password) {
    headers.set('x-admin-password', options.password)
  }

  if (!(options.body instanceof FormData) && options.body && !headers.has('content-type')) {
    headers.set('content-type', 'application/json')
  }

  const response = await fetch(path, {
    ...options,
    headers,
  })

  if (!response.ok) {
    let message = `Request failed with status ${response.status}`

    try {
      const payload = (await response.json()) as { error?: string }
      if (payload.error) {
        message = payload.error
      }
    } catch {
      // Ignore JSON parsing errors and keep the fallback message.
    }

    throw new Error(message)
  }

  return response.blob()
}

export async function login(password: string): Promise<void> {
  await requestJson('/api/admin/login', {
    method: 'POST',
    body: JSON.stringify({ password }),
  })
}

export async function listMusics(password: string): Promise<AdminMusic[]> {
  return requestJson<AdminMusic[]>('/api/admin/musics', {
    password,
  })
}

export async function uploadMusic(
  password: string,
  payload: { file: File; title: string; publicId: string; qualityProfile: StemQualityProfile },
): Promise<AdminMusic> {
  const body = new FormData()
  body.append('file', payload.file)
  body.append('title', payload.title)
  body.append('public_id', payload.publicId)
  body.append('quality_profile', payload.qualityProfile)

  return requestJson<AdminMusic>('/api/admin/musics', {
    method: 'POST',
    password,
    body,
  })
}

export async function retryRender(password: string, id: string): Promise<AdminMusic> {
  return requestJson<AdminMusic>(`/api/admin/musics/${id}/retry`, {
    method: 'POST',
    password,
  })
}

export async function downloadScoreGains(password: string, id: string): Promise<Blob> {
  return requestBlob(`/api/admin/musics/${id}/gains`, {
    password,
  })
}

export async function downloadPublicScoreGains(password: string, accessKey: string): Promise<Blob> {
  return requestBlob(`/api/admin/public/${encodeURIComponent(accessKey)}/gains`, {
    password,
  })
}

export async function exportPublicMixerGains(
  password: string,
  accessKey: string,
  tracks: Array<{ track_index: number; volume_multiplier: number; muted: boolean }>,
): Promise<Blob> {
  return requestBlob(`/api/admin/public/${encodeURIComponent(accessKey)}/gains`, {
    method: 'POST',
    password,
    body: JSON.stringify({ tracks }),
  })
}

export async function updatePublicId(
  password: string,
  id: string,
  publicId: string,
): Promise<AdminMusic> {
  return requestJson<AdminMusic>(`/api/admin/musics/${id}`, {
    method: 'PATCH',
    password,
    body: JSON.stringify({
      public_id: publicId.trim() ? publicId.trim() : null,
    }),
  })
}

export async function fetchPublicMusic(accessKey: string): Promise<PublicMusic> {
  return requestJson<PublicMusic>(`/api/public/${encodeURIComponent(accessKey)}`)
}

export async function fetchStems(accessKey: string): Promise<Stem[]> {
  return requestJson<Stem[]>(`/api/public/${encodeURIComponent(accessKey)}/stems`)
}
