# Fumen

Svelte frontend plus Rust backend for uploading MuseScore `.mscz` files, storing them in S3 or on local disk, and sharing public listening links.

## Included in this MVP

- Hard-coded admin password flow for a private upload screen
- Rust `axum` backend with PostgreSQL metadata storage
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
$env:DATABASE_URL="postgres://postgres:password@127.0.0.1:6432/musescore_reader"
$env:DATABASE_URL_ADMIN="postgres://postgres:password@127.0.0.1:5432/musescore_reader"
$env:DATABASE_URL_READ_ONLY="postgres://postgres:password@127.0.0.1:6433/musescore_reader"
$env:LOCAL_STORAGE_PATH="./data/storage"
$env:S3_REGION="eu-west-3"
$env:S3_ENDPOINT="http://127.0.0.1:9000"
$env:S3_FORCE_PATH_STYLE="true"
$env:MUSESCORE_BIN="C:\Program Files\MuseScore Studio 4\bin\MuseScoreStudio.exe"
$env:MUSESCORE_DOCKER_IMAGE="your-musescore-cli-image"
$env:DOCKER_BIN="docker"
$env:MUSESCORE_QT_PLATFORM="offscreen"
```

If `S3_BUCKET`, `S3_ACCESS_KEY_ID`, and `S3_SECRET_ACCESS_KEY` are all unset, the backend stores uploaded files under `LOCAL_STORAGE_PATH`.

When S3 storage is enabled, uploaded score files and generated derivatives are written as public
objects and the API returns direct object URLs to the frontend. Use a bucket/endpoint that is meant
to be publicly readable. The bucket must also allow browser CORS from the frontend origin if the
browser will fetch MIDI, MusicXML, or stem audio directly.

`DATABASE_URL` is the read-write application connection. `DATABASE_URL_ADMIN` is used for startup
schema management, and `DATABASE_URL_READ_ONLY` is used for public read traffic. If the admin or
read-only URLs are unset, the backend falls back to `DATABASE_URL`.

`MUSESCORE_BIN` enables derivative exports from a native MuseScore install. If it is unset, the
backend also probes common MuseScore locations on Windows, macOS, Linux, and the current `PATH`.

If you prefer containerized conversion, set `MUSESCORE_DOCKER_IMAGE` instead. When it is set, the
backend runs `docker run --rm` with bind mounts for the score input/output directories and expects
the container image entrypoint to behave like the MuseScore CLI. `DOCKER_BIN` overrides the Docker
executable path when `docker` is not the right command name.

On Linux, the backend defaults native MuseScore launches to `QT_QPA_PLATFORM=offscreen` for
headless export. On Windows and macOS it does not force a Qt platform plugin, which avoids the
"no Qt platform plugin could be initialized" failure from native Windows installs. Set
`MUSESCORE_QT_PLATFORM` only when you need to override that default explicitly.

Derivative exports are optional. When MuseScore is available, the backend tries to export:

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
docker build -f Dockerfile.frontend -t fumen-frontend .
```

Build the backend image from the repository root:

```bash
docker build -f Dockerfile.backend -t fumen-backend .
```

Build the soundfonts image from the repository root. The image reads `soundfonts/sources.json`,
downloads each archive, and installs it under the matching key name:

```bash
docker build -f Dockerfile.soundfonts -t fumen-soundfonts .
```

The backend image defaults to:

```bash
BIND_ADDRESS=0.0.0.0:3000
LOCAL_STORAGE_PATH=/data/storage
SOUNDFONT_DIR=/opt/soundfonts
```

For Kubernetes, the intended setup is:

- run `fumen-frontend` as the public web app
- run `fumen-backend` as the API service
- mount the contents of `fumen-soundfonts` into the backend pod at `/opt/soundfonts`

The frontend image serves only static files. Route `/api` to the backend with Ingress or another
cluster-level proxy.

`Dockerfile.backend` includes `ffmpeg`, `fluidsynth`, `sfizz_render`, and MuseScore 4. The image
sets `FLUIDSYNTH_BIN`, `SFIZZ_BIN`, and `MUSESCORE_BIN` automatically.

## Helm deployment

The Helm chart lives in [helm/](/Users/sygmei/Projects/MusescoreReader/helm) and expects a few
cluster dependencies and secrets to exist beforehand.

### Cluster dependencies

- Traefik as the ingress controller
- cert-manager installed in the cluster
- Cloudflare DNS configured for the target domain

### Required secrets

Create these secrets in the target namespace before installing the chart.

`postgresql-credentials` must contain:

```yaml
data:
  connection-string: ...
  connection-string-admin: ...
  connection-string-ro: ...
  database: ...
  host: ...
  host-ro: ...
  password: ...
  port: ...
  username: ...
```

`cloudflare-secret` must contain a `token` key with a Cloudflare API token that can solve DNS01
challenges for the domain:

```bash
kubectl -n <namespace> create secret generic cloudflare-secret \
  --from-literal=token='<cloudflare-api-token>'
```

`s3-creds` must contain the S3 or Spaces credentials used by the backend. With DigitalOcean Spaces,
you can create it like this:

```bash
kubectl -n <namespace> create secret generic s3-creds \
  --from-literal=secret-key='<secret-key>' \
  --from-literal=access-key-id='<access-key-id>' \
  --from-literal=bucket-name='fumen' \
  --from-literal=region-name='fra1' \
  --from-literal=endpoint-url='https://fra1.digitaloceanspaces.com'
```

The chart maps those secret keys to:

- `S3_SECRET_ACCESS_KEY` from `secret-key`
- `S3_ACCESS_KEY_ID` from `access-key-id`
- `S3_BUCKET` from `bucket-name`
- `S3_REGION` from `region-name`
- `S3_ENDPOINT` from `endpoint-url`

### Admin password

The backend still reads `ADMIN_PASSWORD`, but the Helm chart does not inject it yet. If you do not
patch the Deployment, the backend falls back to the default password `fumen-admin`, which is
not safe for production.

### Install

Update [helm/values.yaml](/Users/sygmei/Projects/MusescoreReader/helm/values.yaml) or override the
image tags and domains on the command line, then install:

```bash
helm upgrade --install fumen ./helm \
  --namespace <namespace> \
  --create-namespace
```

The chart will create:

- a cert-manager `Issuer`
- a wildcard-style `Certificate` covering the frontend and backend hosts
- a Traefik HTTPS redirect middleware
- frontend and backend Deployments, Services, and Ingresses

With the current defaults, the public hosts are:

- frontend: `fumen.mydomain.com`
- backend: `fumen-api.mydomain.com`
