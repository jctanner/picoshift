# Release Process for Picoshift

## Context

Users currently must compile both the picoshift CLI (Go), the kind fork (Go), and the ocp-sim container image (Rust) from source. This requires Go 1.26+, Rust toolchain, and rootful podman — a high barrier to entry. We need a GitHub-driven release process that produces pre-built binaries and container images so users can get started with just `kubectl` and `podman`.

## End-User Experience

After this work, a new user's workflow is:

```bash
# Download two binaries from the GitHub Release
curl -LO https://github.com/jctanner/picoshift/releases/download/v2026.06.09/picoshift-linux-amd64
curl -LO https://github.com/jctanner/picoshift/releases/download/v2026.06.09/kind-linux-amd64
chmod +x picoshift-linux-amd64 kind-linux-amd64
sudo mv picoshift-linux-amd64 /usr/local/bin/picoshift
sudo mv kind-linux-amd64 /usr/local/bin/kind

# Create a cluster — no git clone, no compilation
picoshift create
```

**Prerequisites**: `podman` (rootful), `kubectl`, `python3`. No Go, Rust, or source checkout.

All deploy manifests (CRDs, seed resources, simulator YAML, scripts) are embedded inside the `picoshift` binary via Go's `//go:embed`. Container images (`ocp-sim`, `kindest-node`) are pulled from ghcr.io automatically.

## Versioning

CalVer with format **`YYYY.MM.DD`** (e.g. `2026.06.09`). Git tags are prefixed with `v` (e.g. `v2026.06.09`). Image tags and CLI version output omit the `v` prefix.

If a same-day hotfix is needed, append a patch counter: `2026.06.09.1`.

## Release Artifacts

On each tagged release (`v*`), CI will produce:

**GitHub Release attachments (linux/amd64):**
- `picoshift-linux-amd64` — the picoshift CLI binary (with embedded deploy assets)
- `kind-linux-amd64` — the forked kind binary (with ocp-shim support)
- `checksums.txt` — SHA256 checksums

**Container images pushed to ghcr.io:**
- `ghcr.io/jctanner/picoshift/ocp-sim:2026.06.09` — the Rust simulator
- `ghcr.io/jctanner/picoshift/kindest-node:2026.06.09` — kind node image with ocp-shim baked in

## Changes Required

### 1. Embed deploy assets into the CLI binary

**Files:** `cli/embed.go` (NEW), `cli/internal/config.go` (MODIFY)

Use Go's `//go:embed` to bundle the `deploy/` and `scripts/` directories into the picoshift binary. Total embedded size is ~1.3 MB.

```go
// cli/embed.go
package main

import "embed"

//go:embed deploy scripts
var embeddedAssets embed.FS
```

Note: Go's `//go:embed` resolves paths relative to the source file. Since `deploy/` and `scripts/` live in the project root but the CLI source is in `cli/`, the CI build step will copy `deploy/` and `scripts/` into `cli/` before compiling. The Makefile's `build-cli` target will handle this. In dev mode (`Version == "dev"`), the CLI ignores embedded assets and uses the project root paths as it does today.

At runtime, when the CLI detects it's in release mode (no project root found, or `Version != "dev"`), it extracts the embedded assets to `~/.cache/picoshift/<version>/` on first use and references those paths. Subsequent runs reuse the cache. The existing `ProjectRoot()` function is updated to fall back to the cache directory.

### 2. Make image names configurable in the CLI

**File:** `cli/internal/config.go`

Add registry-aware image resolution. When `Version == "dev"`, use localhost images (local build mode). When it's a release version, default to ghcr.io images:

```go
const (
    DefaultRegistry = "ghcr.io/jctanner/picoshift"
    LocalNodeImage  = "localhost/kindest/node:ocp-shim"
    LocalSimImage   = "localhost/ocp-sim:latest"
)

func NodeImage(version string) string   // "dev" → local, else ghcr.io/.../kindest-node:<version>
func SimImageRef(version string) string  // "dev" → local, else ghcr.io/.../ocp-sim:<version>
func IsDevMode(version string) bool      // version == "dev" or version == ""
```

### 3. Update `picoshift create` for release mode

**File:** `cli/cmd/create.go`

Two key changes:

**Image loading**: Currently `deploySim()` does `podman save | ctr import` for locally-built images. In release mode:
- Pull the ghcr.io image into the kind node via `podman exec ctr images pull` (or `podman save` the registry image and pipe it in)
- Patch the DaemonSet image reference to the ghcr.io tag after applying the manifest
- Set `imagePullPolicy: IfNotPresent` instead of `Never`

**Kind binary lookup**: Currently hardcoded to `deps/kind/bin/kind`. In release mode:
- Look for `kind` on `$PATH`
- Fall back to `deps/kind/bin/kind` for dev mode
- Update `checkDeps()` accordingly

### 4. GitHub Actions release workflow (NEW)

**File:** `.github/workflows/release.yml`

Triggered on push of tags matching `v*`. Three jobs:

**Job 1: `build-cli`** — Build picoshift + kind binaries
- Check out this repo
- Check out `jctanner/kind` at `OCP_SHIM` branch
- Set up Go 1.26
- Copy `deploy/` and `scripts/` into `cli/` for embedding
- Build `picoshift` with `-ldflags "-X main.Version=$TAG"` from `cli/`
- Cross-compile `kind` binary from the fork checkout
- Upload both as release artifacts

**Job 2: `build-images`** — Build and push container images
- Check out this repo
- Check out `jctanner/kind` at `OCP_SHIM` branch
- Set up Docker buildx
- Log in to ghcr.io via `GITHUB_TOKEN`
- Build + push `ocp-sim` image from `simulator/Dockerfile`
- Build the kind base image (with ocp-shim), then use the kind binary to build + push the node image

**Job 3: `release`** (depends on jobs 1 & 2)
- Download CLI artifacts from job 1
- Generate SHA256 checksums
- Create GitHub Release with binaries + checksums attached
- Auto-generate release notes from commits

### 5. Version injection for CLI builds

**File:** `Makefile` (build-cli target)

Update the `build-cli` target to inject version via ldflags:

```makefile
VERSION ?= dev
build-cli:
	cd cli && go build -ldflags "-X main.Version=$(VERSION)" -o ../bin/picoshift .
```

### 6. Update Makefile image variables

**File:** `Makefile`

`NODE_IMAGE` and `SIM_IMAGE` are already `?=` assignments so they can be overridden. No changes needed beyond the `build-cli` ldflags addition.

## Two Modes of Operation

| Aspect | Dev mode (`Version == "dev"`) | Release mode (`Version == "2026.06.09"`) |
|--------|-------------------------------|--------------------------------------|
| Deploy assets | `deploy/` and `scripts/` from project root | Extracted from embedded FS to `~/.cache/picoshift/2026.06.09/` |
| Kind binary | `deps/kind/bin/kind` | `kind` on `$PATH` or next to `picoshift` binary |
| Sim image | `localhost/ocp-sim:latest` (local podman build) | `ghcr.io/jctanner/picoshift/ocp-sim:2026.06.09` |
| Node image | `localhost/kindest/node:ocp-shim` (local kind build) | `ghcr.io/jctanner/picoshift/kindest-node:2026.06.09` |
| Entra-mock image | `localhost/entra-mock:latest` (local podman build) | `ghcr.io/jctanner/picoshift/entra-mock:2026.06.09` |
| Image loading | `podman save \| ctr import` | `podman pull` + retag + `podman save \| ctr import` |
| `picoshift build` | Builds all images locally | No-op (prints message: "images are pre-built") |
| `picoshift create --build` | Builds images before creating cluster | Ignored with message (images are pre-built) |

## File Summary

| File | Action | Description |
|------|--------|-------------|
| `.github/workflows/release.yml` | NEW | CI workflow: build binaries, build+push images, create GH release |
| `cli/embed_release.go` | NEW | `//go:embed` directive for deploy/ and scripts/ (gated by `embed_assets` build tag) |
| `cli/embed_dev.go` | NEW | Empty embed.FS for dev builds (default, no build tag) |
| `cli/internal/config.go` | MODIFY | Registry-aware image functions, embedded asset extraction, version-based mode |
| `cli/cmd/create.go` | MODIFY | Release mode: pull registry images, find kind on PATH, use extracted assets |
| `cli/cmd/build.go` | MODIFY | No-op when in release mode |
| `cli/main.go` | NO CHANGE | Already has `var Version = "dev"` |
| `deploy/simulator.yaml` | NO CHANGE | CLI patches image at deploy time |
| `Makefile` | MODIFY | Add `-ldflags` to build-cli, copy deploy/scripts into cli/ for embed |

## Verification

1. **Dev flow unchanged**: `make build-cli && bin/picoshift build && bin/picoshift create --build` works identically to today
2. **Release CI**: Push tag `v2026.06.09` → GH Actions builds → Release appears with `picoshift-linux-amd64`, `kind-linux-amd64`, `checksums.txt` → ghcr.io has `ocp-sim:2026.06.09`, `kindest-node:2026.06.09`, and `entra-mock:2026.06.09`
3. **End-user flow**: Download binaries, run `picoshift create` → assets extracted to `~/.cache/picoshift/2026.06.09/`, cluster created with ghcr.io images, no source checkout or compilation needed
4. **Embedded assets**: `picoshift version` shows the release version; `picoshift create` without a project root works using embedded CRDs, seed resources, and scripts
