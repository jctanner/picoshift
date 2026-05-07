# OCP Lite Simulator — Implementation Plan

## Context

OpenShift (OCP) is heavy, slow to install, and resource-intensive. For RHOAI/OpenDataHub operator development and testing, only a small subset of OCP's APIs are actually needed. This project creates a lightweight simulator that runs on `kind` (Kubernetes-in-Docker) with OpenShift CRD stubs and a small Rust-based controller that provides dynamic behavior (e.g., Route status updates).

**First target**: The opendatahub-operator binary starts, detects "OpenShift", and enters its reconcile loop without crashing.

**Stack**: kind cluster (~200MB RAM) + OpenShift CRDs (YAML) + Rust simulator binary (kube-rs)

---

## Project Structure

```
ocp-rhoai-simulator/
├── kind/
│   └── cluster.yaml              # kind cluster configuration
├── crds/
│   ├── fetch-crds.sh             # Script to extract CRDs from openshift/api + operator-framework/api
│   ├── openshift/                # OpenShift API CRDs (generated/fetched)
│   └── olm/                     # OLM CRDs (generated/fetched)
├── seed/
│   ├── clusterversion.yaml       # ClusterVersion "version" resource
│   ├── sccs.yaml                 # Default SecurityContextConstraints
│   ├── authentication.yaml       # config.openshift.io Authentication
│   ├── ingress.yaml              # config.openshift.io Ingress
│   ├── infrastructure.yaml       # config.openshift.io Infrastructure
│   └── cluster-config.yaml       # kube-system/cluster-config-v1 ConfigMap
├── simulator/                    # Rust project (kube-rs)
│   ├── Cargo.toml
│   └── src/
│       └── main.rs               # Watches Routes, updates status; other mock controllers
├── scripts/
│   ├── setup.sh                  # Full setup: create cluster → install CRDs → seed → deploy sim
│   └── teardown.sh               # Destroy cluster
├── Makefile                      # Top-level orchestration
└── example.src/                  # Reference source (opendatahub-operator, microshift)
```

---

## Milestone 1: Operator Binary Starts (Current Target)

### Step 1: kind cluster configuration
- Single-node kind cluster
- Expose ports for API server access
- File: `kind/cluster.yaml`

### Step 2: Fetch/generate OpenShift CRDs
The ODH operator registers these OCP API groups in its scheme (`cmd/main.go` lines 146-178). We need CRDs for each so the API server recognizes them:

**From `github.com/openshift/api`** (match version in ODH's go.mod):
1. `config.openshift.io/v1` — ClusterVersion, Authentication, Ingress, Infrastructure
2. `route.openshift.io/v1` — Route
3. `security.openshift.io/v1` — SecurityContextConstraints
4. `image.openshift.io/v1` — ImageStream, ImageStreamTag
5. `oauth.openshift.io/v1` — OAuthClient
6. `console.openshift.io/v1` — ConsoleLink (+ OdhQuickStart is ODH's own CRD)
7. `operator.openshift.io/v1` — IngressController
8. `build.openshift.io/v1` — BuildConfig, Build
9. `template.openshift.io/v1` — Template
10. `user.openshift.io/v1` — User, Group
11. `apps.openshift.io/v1` — DeploymentConfig
12. `machine.openshift.io/v1beta1` — MachineSet, MachineAutoscaler

**From `github.com/operator-framework/api`**:
13. `operators.coreos.com/v1alpha1` — Subscription, ClusterServiceVersion, CatalogSource, InstallPlan
14. `operators.coreos.com/v2` — OperatorCondition

**Approach**: We already have ready-made CRD YAMLs from two sources:

- **MicroShift** provides CRDs for Route, SCC, and storage migration at `example.src/microshift/assets/crd/`
- **MicroShift OLM** provides CRDs for all OLM types (CSV, Subscription, CatalogSource, etc.) at `example.src/microshift/assets/optional/operator-lifecycle-manager/`
- **MicroShift SCC seed data** provides all default SCC instances at `example.src/microshift/assets/controllers/openshift-default-scc-manager/`
- **ODH operator** has a `fetch-external-crds` Makefile function that uses `controller-gen` to extract CRDs from the `openshift/api` Go module — we can adapt this approach for the remaining API groups (config, image, oauth, console, operator, build, template, user, apps, machine)

For CRDs not bundled in MicroShift, we'll write `crds/fetch-crds.sh` that uses `controller-gen` against the `openshift/api` Go module at the version pinned in ODH's `go.mod` (`v0.0.0-20230823114715-5fdd7511b790`). Alternatively, for types that don't have Go struct CRD markers, we'll write minimal hand-crafted CRDs (just enough for API server to accept resources of that type).

### Step 3: Create seed resources
Pre-populate resources the operator queries during `cluster.Init()`:

1. **ClusterVersion "version"** — `config.openshift.io/v1` — This is the **primary OpenShift detection trigger**. The operator fetches this by name at startup (`pkg/cluster/cluster_config.go`). Must include `.status.desired.version`.

2. **cluster-config-v1 ConfigMap** in `kube-system` — Contains `install-config` key with FIPS setting. Operator reads this for FIPS detection (graceful fallback if missing, but cleaner to provide it).

3. **Default SCCs** — `restricted`, `anyuid`, `privileged`, etc. The operator creates/patches SCCs during DSCI reconciliation but expects the CRD to exist at startup.

4. **Authentication** — `config.openshift.io/v1` named `cluster` — Operator reads this for OIDC configuration.

5. **Ingress** — `config.openshift.io/v1` named `cluster` — Operator reads cluster domain from this.

6. **Infrastructure** — `config.openshift.io/v1` named `cluster` — Used for topology detection (single-node vs multi-node).

### Step 4: Install ODH operator CRDs
The operator's own CRDs (DSC, DSCI, component CRDs) from `example.src/opendatahub-operator/config/crd/`.

### Step 5: Deploy the operator
Run the ODH operator binary against the kind cluster's kubeconfig. For milestone 1, run it as a local process (not in-cluster) pointing at the kind cluster. Success = the process starts, logs show "OpenShift" detection, and it enters the reconcile loop waiting for DSC/DSCI resources.

### Step 6: Setup script
`scripts/setup.sh` orchestrates steps 1-5:
```
kind create cluster --config kind/cluster.yaml
kubectl apply -f crds/openshift/
kubectl apply -f crds/olm/
kubectl apply -f seed/
# Install ODH CRDs from example.src
kubectl apply -f example.src/opendatahub-operator/config/crd/bases/
```

---

## Milestone 2: Rust Simulator Controller (Future)

After the operator starts, it will try to reconcile DSC/DSCI resources and deploy components. Components create Routes, reference SCCs, etc. These need dynamic status updates to appear "ready."

### Simulator responsibilities:
1. **Route controller** — Watch `route.openshift.io/v1/Route`, set `.status.ingress` with the route's hostname and "Admitted" condition
2. **ClusterVersion controller** — Keep ClusterVersion status updated with version info and "Available" condition
3. **IngressController stub** — Respond to IngressController reads with a default config

### Rust project setup:
- `kube-rs` with `runtime` feature for controller pattern
- `k8s-openapi` for standard K8s types
- `DynamicObject` from kube-rs for OpenShift types (no need to port Go structs)
- Single binary, <5MB, <10MB RAM at runtime

---

## Milestone 3: Operator Reconciles CRs (Future)

- Create DSCI and DSC resources
- Operator reconciles them, creates component CRs
- Component controllers try to render/deploy manifests
- Most will partially succeed (Deployments created but pods may not have images)
- Dashboard might fully deploy if its container images are pullable

---

## Verification (Milestone 1)

1. `kind get clusters` shows the simulator cluster
2. `kubectl get crd | grep openshift` shows all OCP CRDs installed
3. `kubectl get clusterversion version` returns the seed ClusterVersion
4. Run the ODH operator with `--kubeconfig` pointing at kind cluster
5. Operator logs show:
   - "Cluster type: OpenShift"
   - No fatal errors
   - Reconcilers registered and waiting

---

## Key Source Files

| Purpose | Path |
|---------|------|
| ODH operator entrypoint | `example.src/opendatahub-operator/cmd/main.go` |
| Cluster detection logic | `example.src/opendatahub-operator/pkg/cluster/cluster_config.go` |
| GVK definitions (~200) | `example.src/opendatahub-operator/pkg/cluster/gvk/gvk.go` |
| ODH CRDs | `example.src/opendatahub-operator/config/crd/bases/` |
| CRD fetch function | `example.src/opendatahub-operator/Makefile` (`fetch-external-crds`) |
| MicroShift Route + SCC CRDs | `example.src/microshift/assets/crd/` |
| MicroShift OLM CRDs | `example.src/microshift/assets/optional/operator-lifecycle-manager/` |
| MicroShift default SCCs | `example.src/microshift/assets/controllers/openshift-default-scc-manager/` |

## CRD Source Strategy

| API Group | CRD Source | Seed Data Needed? |
|-----------|-----------|-------------------|
| `route.openshift.io` | MicroShift `assets/crd/route.crd.yaml` | No |
| `security.openshift.io` | MicroShift `assets/crd/...securitycontextconstraints.crd.yaml` | Yes (SCCs from MicroShift) |
| `operators.coreos.com` | MicroShift `assets/optional/operator-lifecycle-manager/*.crd.yaml` | No |
| `config.openshift.io` | Generate via `controller-gen` from `openshift/api` | Yes (ClusterVersion, Auth, Ingress, Infra) |
| `image.openshift.io` | Generate or hand-craft minimal CRD | No |
| `oauth.openshift.io` | Generate or hand-craft minimal CRD | No |
| `console.openshift.io` | Generate or hand-craft minimal CRD | No |
| `operator.openshift.io` | Copy from ODH's `fetch-external-crds` output (IngressController) | No |
| `build.openshift.io` | Generate or hand-craft minimal CRD | No |
| `template.openshift.io` | Generate or hand-craft minimal CRD | No |
| `user.openshift.io` | Generate or hand-craft minimal CRD | No |
| `apps.openshift.io` | Generate or hand-craft minimal CRD | No |
| `machine.openshift.io` | Generate or hand-craft minimal CRD | No |

## Risks & Open Questions

1. **config.openshift.io CRDs**: ClusterVersion, Authentication, Ingress, and Infrastructure are not standard CRDs in real OCP — they're built into the API server. We may need to hand-craft CRD YAMLs for these since `controller-gen` may not produce them from the Go types. Fallback: write minimal CRDs by hand.

2. **ClusterVersion status subresource**: The operator reads `.status.desired.version`. We need the CRD to have a status subresource, and we need a way to set status (either via `kubectl` status patch or our seed script uses the API directly).

3. **Dynamic REST mapper**: The operator uses `apiutil.NewDynamicRESTMapper` which discovers APIs at runtime. As long as CRDs are installed and established, this should work on kind.

4. **Operator RBAC**: The operator needs extensive RBAC for all the OCP resource types. We'll need a comprehensive ClusterRole. Can adapt from `example.src/opendatahub-operator/config/rbac/role.yaml`.
