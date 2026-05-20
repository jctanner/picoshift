# picoshift CLI — Design Plan

## Motivation

The project currently uses a Makefile with 40+ targets and shell scripts for
cluster lifecycle management. This works but has real ergonomic problems:

- **Discoverability** — new users must read the Makefile to understand what's
  available. `make help` exists but is a wall of text with no grouping.
- **Composability** — passing options requires Make variable syntax
  (`make deploy-sim AUTH_MODE=byoidc OIDC_ISSUER_URL=...`), which is awkward
  and error-prone.
- **State awareness** — the Makefile is stateless. It can't tell you whether
  the cluster is running, what auth mode was used, or whether the gateway stack
  is installed.
- **Ecosystem fit** — kind, minikube, and crc all have dedicated CLIs. A
  `picoshift` command would feel natural alongside them.

## Goals

1. Single binary (`picoshift`) that replaces the Makefile for all cluster
   lifecycle operations
2. Subcommand structure similar to kind/minikube (`picoshift create`,
   `picoshift delete`, `picoshift status`)
3. Shell out to existing tools (kubectl, podman, helm, istioctl) — the CLI is
   an orchestration layer, not a reimplementation
4. Embed deploy manifests (CRDs, seed, simulator.yaml) so the CLI is
   self-contained after build
5. The Makefile remains as a thin wrapper for development convenience but is no
   longer the primary interface

## Non-goals

- Replacing the Rust simulator binary — that stays as-is
- Reimplementing kubectl/podman/helm logic in Go
- Supporting clusters other than kind (for now)

## Command structure

```
picoshift
├── init              Clone/update required deps (kind fork, ODH operator, entra-mock)
├── build             Build all images (kind CLI, base, node, simulator)
│   ├── --kind        Build only kind CLI + images
│   └── --sim         Build only simulator image
├── create            Create cluster + deploy everything (replaces `make all`)
│   ├── --auth-mode   legacy|oidc|byoidc (default: legacy)
│   ├── --name        Cluster name (default: ocp-sim)
│   └── --no-deploy   Create cluster only, skip CRDs/seed/simulator
├── delete            Delete the cluster (replaces `make teardown`)
│   └── --name        Cluster name
├── deploy            Deploy components to an existing cluster
│   ├── crds          Apply CRDs
│   ├── seed          Apply seed resources
│   ├── sim           Build + deploy simulator
│   ├── gateway-stack Install Istio + cert-manager + Kuadrant
│   ├── operator      Build + deploy ODH operator + DSCI + DSC
│   ├── entra-mock    Deploy Entra ID emulator
│   └── maas          Deploy MaaS CRDs + seed
├── redeploy          Rebuild and redeploy simulator (preserves cluster)
├── status            Show cluster state, pods, auth mode, installed components
├── logs              Tail simulator logs (replaces `make logs`)
│   ├── --sim         Simulator logs (default)
│   └── --operator    Operator logs
├── workbench         Create a workbench
│   ├── --project     Project name (default: project1)
│   ├── --name        Workbench name (default: workbench1)
│   └── --image       Notebook image
└── version           Print CLI and cluster version info
```

## Implementation

### Language and dependencies

- **Go** — matches the kind/minikube/kubectl ecosystem, single static binary,
  the project already has Go code in the ocp-shim
- **cobra** — standard CLI framework (used by kubectl, kind, minikube, helm)
- **embed** — Go 1.16+ `//go:embed` to bundle deploy manifests into the binary

### Project layout

```
cli/
├── go.mod
├── go.sum
├── main.go                 # cobra root command + version
├── cmd/
│   ├── init.go             # picoshift init
│   ├── build.go            # picoshift build
│   ├── create.go           # picoshift create
│   ├── delete.go           # picoshift delete
│   ├── deploy.go           # picoshift deploy [subcommand]
│   ├── redeploy.go         # picoshift redeploy
│   ├── status.go           # picoshift status
│   ├── logs.go             # picoshift logs
│   ├── workbench.go        # picoshift workbench
│   └── version.go          # picoshift version
├── internal/
│   ├── exec.go             # shell-out helpers (run kubectl, podman, helm, etc.)
│   ├── cluster.go          # cluster state detection (is it running? what auth mode?)
│   ├── images.go           # image build + load helpers
│   └── config.go           # cluster config (name, auth mode, paths)
└── embed/
    └── manifests.go        # //go:embed for deploy/ directory
```

### Execution model

Each command composes a sequence of shell-outs, similar to what the Makefile
does today. For example, `picoshift create --auth-mode oidc` would:

1. Check that dependencies exist (`deps/kind`, podman, kubectl)
2. Build images if not present (kind CLI, base, node, simulator)
3. Run `kind create cluster --config deploy/kind/cluster.yaml`
4. Export kubeconfig
5. Apply CRDs from embedded manifests
6. Apply seed resources (choosing auth YAML based on `--auth-mode`)
7. Create ClusterVersion with status subresource
8. Build + load + deploy simulator with auth mode flags
9. Set up admin RBAC
10. Print summary with next steps

The `internal/exec.go` helper would handle:
- Running commands with stdout/stderr streaming
- Prefixing output with step numbers (like rebuild.sh does)
- Failing fast with clear error messages
- Dry-run mode (`--dry-run` prints commands without executing)

### State detection

`picoshift status` would query the cluster directly:

- Is the kind cluster running? (`kind get clusters`)
- Is the simulator pod ready? (`kubectl get pods -n ocp-sim`)
- What auth mode is active? (read DaemonSet args)
- Is the gateway stack installed? (check for istio-system namespace)
- Is the ODH operator running? (check for operator namespace)
- Is entra-mock deployed? (check for entra-mock namespace)

### Embedded manifests

Using `//go:embed`, the CLI bundles the CRDs, seed resources, and deployment
manifests at build time. This means `picoshift create` works without needing
the full repo checkout — just the binary and deps/.

```go
//go:embed deploy/crds deploy/seed deploy/simulator.yaml deploy/kind
var manifests embed.FS
```

Applied via: write to temp file, `kubectl apply -f <tempfile>`.

## Migration path

1. **Phase 1**: Build the CLI with core commands (init, build, create, delete,
   status, logs). Keep the Makefile as-is.
2. **Phase 2**: Add deploy subcommands (gateway-stack, operator, workbench,
   entra-mock, maas). The CLI becomes the recommended interface.
3. **Phase 3**: Thin the Makefile to development shortcuts that call the CLI
   (`make all` → `picoshift create`).

## Build and install

The CLI builds to `bin/picoshift` at the project root. `bin/` is added to
`.gitignore` (same pattern as the kind fork's build output).

```bash
# Build
cd cli && go build -o ../bin/picoshift .

# Or via Makefile
make build-cli

# Install to PATH
sudo cp bin/picoshift /usr/local/bin/
```

## Stretch goal: zero-dependency cluster creation

Eventually, a user should be able to download the `picoshift` binary and run
`picoshift create` with no repo checkout, no deps/, no local image builds. The
cluster would pull pre-built images from a public registry.

This means the CLI should be designed so that:

- **Manifests are embedded**, not read from disk (already planned via
  `//go:embed`). The binary is self-contained.
- **Image references are configurable**, not hardcoded to `localhost/`. A
  `--image-registry` flag or config file would let the CLI point at
  `ghcr.io/jctanner/picoshift/` for pre-built images instead of expecting
  local builds.
- **`picoshift init` and `picoshift build` become optional** — only needed for
  development. `picoshift create` should work without them when using published
  images.
- **The kind binary is vendored or downloaded** — either bundle a kind build
  into the picoshift binary itself, or download it on first run (like minikube
  does with its drivers).
- **ocp-shim is baked into the published node image** — no need to build the
  kind fork locally if the node image is pre-built and hosted.

The phased approach supports this naturally: Phase 1-2 work with local builds
(current workflow), Phase 3+ adds registry support and published images. The
key architectural decision is keeping image references and manifest paths
behind an abstraction from the start, so the transition from "build locally"
to "pull from registry" is a config change, not a rewrite.

## Example session

```bash
# First time setup
picoshift init
picoshift build
picoshift create

# Iterate on simulator
picoshift redeploy

# Add gateway stack + operator
picoshift deploy gateway-stack
picoshift deploy operator

# Check state
picoshift status

# BYOIDC mode
picoshift delete
picoshift create --auth-mode byoidc

# Tear down
picoshift delete
```
