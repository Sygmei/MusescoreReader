# MusescoreReader

Svelte frontend plus Rust backend for uploading MuseScore `.mscz` files, storing them in S3 or on local disk, and sharing public listening links.

## Included in this MVP

- Hard-coded admin password flow for a private upload screen
- Rust `axum` backend with SQLite metadata storage
- Storage backend that uses S3 when configured and falls back to local filesystem storage otherwise
- Random public link for each upload
- Optional friendly public id per score
- Automatic `.mscz -> .mid` export during upload when MuseScore CLI is available
- Optional audio preview export for browser playback when MuseScore CLI is available
- Public page where anonymous users can use a browser-based MIDI mixer, adjust instrument volumes, and download the original score or MIDI export

## Layout

- `frontend/`: Vite + Svelte app
- `backend/`: Rust API server

## Environment

Set these before running the backend:

```powershell
$env:ADMIN_PASSWORD="change-me"
```

If you want S3 storage, set all three of these:

```powershell
$env:S3_BUCKET="your-bucket"
$env:S3_ACCESS_KEY_ID="minioadmin"
$env:S3_SECRET_ACCESS_KEY="minioadmin"
```

Optional settings:

```powershell
$env:APP_BASE_URL="http://localhost:5173"
$env:BIND_ADDRESS="127.0.0.1:3000"
$env:DATABASE_PATH="./data/musescore-reader.db"
$env:LOCAL_STORAGE_PATH="./data/storage"
$env:S3_REGION="eu-west-3"
$env:S3_ENDPOINT="http://127.0.0.1:9000"
$env:S3_FORCE_PATH_STYLE="true"
$env:MUSESCORE_BIN="C:\Program Files\MuseScore Studio 4\bin\MuseScoreStudio.exe"
```

If `S3_BUCKET`, `S3_ACCESS_KEY_ID`, and `S3_SECRET_ACCESS_KEY` are all unset, the backend stores uploaded files under `LOCAL_STORAGE_PATH`.

`MUSESCORE_BIN` enables derivative exports. Browsers cannot natively play `.mscz`, so the backend tries to export:

- MP3 for direct browser playback
- MIDI for browser-side playback and per-instrument frontend mixing

If the MuseScore CLI is not available, the upload still succeeds and the public page still offers the original `.mscz` download.

MuseScore's official command-line docs describe `-o output.mid input.mscz` and similar export modes here: [Command line options](https://musescore.org/de/print/book/export/html/278640).

## Run locally

Install frontend dependencies:

```powershell
cd frontend
npm.cmd install
```

Start the backend:

```powershell
cd backend
cargo run
```

Start the frontend dev server:

```powershell
cd frontend
npm.cmd run dev
```

The Vite dev server proxies `/api` to `http://127.0.0.1:3000`.

## Production build

Build the frontend first:

```powershell
cd frontend
npm.cmd run build
```

Then run the backend. If `frontend/dist` exists, the Rust server also serves the compiled SPA.

## Docker images

This repository now includes three Docker build targets:

- `Dockerfile.frontend` builds the Svelte app into a small static image served by Nginx
- `Dockerfile.backend` builds the Rust API server into a runtime image
- `Dockerfile.soundfonts` builds a read-only asset image for the soundfont bundle

Build the frontend image from the repository root:

```bash
docker build -f Dockerfile.frontend -t musescore-reader-frontend .
```

Build the backend image from the repository root:

```bash
docker build -f Dockerfile.backend -t musescore-reader-backend .
```

Build the soundfonts image from the repository root. The image reads `soundfonts/sources.json`,
downloads each archive, and installs it under the matching key name:

```bash
docker build -f Dockerfile.soundfonts -t musescore-reader-soundfonts .
```

The backend image defaults to:

```bash
BIND_ADDRESS=0.0.0.0:3000
DATABASE_PATH=/data/musescore-reader.db
LOCAL_STORAGE_PATH=/data/storage
SOUNDFONT_DIR=/opt/soundfonts
```

For Kubernetes, the intended setup is:

- run `musescore-reader-frontend` as the public web app
- run `musescore-reader-backend` as the API service
- mount the contents of `musescore-reader-soundfonts` into the backend pod at `/opt/soundfonts`

The frontend image serves only static files. Route `/api` to the backend with Ingress or another
cluster-level proxy.

`Dockerfile.backend` includes `ffmpeg` and `fluidsynth`. If you want MIDI export and stem rendering
fully inside that image, you should extend it to also provide `MUSESCORE_BIN` and `SFIZZ_BIN`.
