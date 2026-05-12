# picoshift

A lightweight OpenShift simulator that runs on [kind](https://kind.sigs.k8s.io/).
It provides just enough of the OCP control plane for operators like
[opendatahub-operator](https://github.com/opendatahub-io/opendatahub-operator)
and the ODH Dashboard to start, reconcile, and serve a working UI — all on a
single laptop with ~200 MB of RAM.

```mermaid
graph TB
    browser["Browser / oc CLI"]

    subgraph kind ["kind cluster (single node)"]
        subgraph shim ["ocp-shim"]
            apiserver["kube-apiserver"]
            ocpshim["ocp-shim proxy<br/><i>OCP discovery endpoints</i>"]
            ocpshim --> apiserver
        end

        subgraph sim ["ocp-sim (DaemonSet)"]
            proxy["Reverse Proxy<br/><i>*.apps.ocp-sim.test</i>"]
            routectrl["Route Controller"]
            oauthctrl["OAuth Server<br/><i>login form + users.yaml</i>"]
            projectctrl["Project Controller"]
            serviceca["Service CA"]
            sccwebhook["SCC Webhook<br/><i>MutatingAdmission</i>"]
            jobsetctrl["JobSet Controller"]
            isctrl["ImageStream Controller"]
            lbctrl["LoadBalancer Controller"]
        end

        subgraph gateway ["Gateway (openshift-ingress)"]
            istiogw["Istio Gateway<br/><i>Envoy + EnvoyFilter</i>"]
            kubeauthproxy["kube-auth-proxy<br/><i>oauth2-proxy</i>"]
        end

        subgraph istiostack ["Istio + Kuadrant"]
            istiod["istiod<br/><i>rev: openshift-gateway</i>"]
            authorino["Authorino"]
            limitador["Limitador"]
        end

        subgraph workloads ["Operator Workloads"]
            odhop["opendatahub-operator"]
            dashboard["ODH Dashboard<br/><i>+ kube-rbac-proxy sidecar</i>"]
            notebook["Workbench Pod<br/><i>Jupyter + kube-rbac-proxy</i>"]
            maas["MaaS API + Controller"]
        end

        crds[("OpenShift CRDs<br/>+ seed resources")]

        routectrl -->|"watch Routes<br/>stamp .status"| apiserver
        projectctrl -->|"mirror Namespaces<br/>→ Projects"| apiserver
        serviceca -->|"inject TLS certs"| apiserver
        sccwebhook -->|"mutate pods<br/>inject fsGroup"| apiserver
        jobsetctrl -->|"watch JobSets<br/>create child Jobs"| apiserver
        isctrl -->|"watch ImageStreams<br/>populate status.tags"| apiserver
        lbctrl -->|"assign IPs to<br/>LB Services"| apiserver

        odhop -->|"reconcile CRs"| ocpshim
        istiod -->|"configure"| istiogw
        istiogw -->|"ext_authz<br/>check session"| kubeauthproxy
        kubeauthproxy -.->|"OAuth redirect<br/>(via browser)"| oauthctrl

        istiogw -->|"route traffic"| dashboard
        istiogw -->|"route traffic"| notebook
        istiogw -->|"route traffic"| maas
    end

    browser -->|":443"| proxy
    proxy -->|"rh-ai.*"| istiogw
    proxy -->|"oauth-openshift.*"| oauthctrl
```

## Why

Real OpenShift clusters are heavy, slow to provision, and expensive to keep
around for day-to-day development. Most RHOAI/ODH operator code only touches a
handful of OpenShift-specific APIs. picoshift stubs those APIs with CRDs on a
plain kind cluster and runs a small Rust binary (`ocp-sim`) that handles the
dynamic parts: Route admission, Gateway API / Envoy xDS, OAuth flow, TLS
certificate injection, LoadBalancer IP assignment, and Namespace → Project
mirroring.

## What's in the box

| Component | What it does |
|-----------|-------------|
| **kind cluster** | Single-node Kubernetes with port mappings for 80/443/9443 |
| **ocp-shim** | Go sidecar baked into the kind node image — intercepts the API server to serve OpenShift discovery endpoints (`/apis/project.openshift.io`, `/.well-known/oauth-authorization-server`, etc.) |
| **OpenShift CRDs** | Routes, Projects, SCCs, ClusterVersion, OLM types, Gateway API, JobSet, and more |
| **Seed resources** | ClusterVersion, Infrastructure, Ingress, Authentication, default SCCs, JobSet operator — everything operators probe at startup |
| **ocp-sim** | Rust controller (kube-rs) running as a DaemonSet with `hostNetwork: true` |
| **Istio** | Service mesh with `openshift-gateway` revision — provides Gateway API data plane (Envoy) |
| **kube-auth-proxy** | oauth2-proxy deployment in `openshift-ingress` — manages browser sessions, wired into Envoy via ext_authz EnvoyFilter, redirects unauthenticated users to the OAuth server |
| **Kuadrant** | Authorino + Limitador for gateway auth policies and rate limiting |
| **cert-manager** | TLS certificate management for gateway listeners |

### ocp-sim controllers

- **Route** — watches `route.openshift.io/v1` Routes, stamps `.status.ingress` with the admitted hostname
- **OAuth** — serves `/oauth/authorize` (login form + Basic Auth challenge for `oc login`) and `/oauth/token`; authenticates against `users.yaml`, creates `User` and `Identity` API objects on first login, and returns per-user groups via `/oauth/userinfo`
- **Project** — mirrors every Namespace as a `project.openshift.io/v1` Project
- **Service CA** — injects TLS certificates into annotated Services and their corresponding Secrets
- **SCC Webhook** — mutating admission webhook that injects `fsGroup` into pods with `runAsNonRoot: true`, mimicking OCP's restricted SCC; also annotates namespaces with UID ranges
- **JobSet** — partial mock of the `jobset.x-k8s.io/v1alpha2` controller; creates real `batch/v1` child Jobs from `spec.replicatedJobs` and tracks completion status
- **ImageStream** — watches `image.openshift.io/v1` ImageStreams and populates `status.tags` from `spec.tags`, resolving `dockerImageReference` so the ODH Dashboard and notebook webhook can look up workbench images
- **LoadBalancer** — assigns node IPs to Services of type LoadBalancer, replacing the need for MetalLB on kind
- **Proxy** — reverse proxy for Route and HTTPRoute hostnames (resolves `*.apps.ocp-sim.test` to the right backend Service); supports WebSocket upgrade tunneling for Jupyter kernel connections

## Requirements

- Linux (tested on Fedora 43)
- Podman (rootful — `sudo`)
- Go 1.26+
- Rust toolchain (for building the simulator image)
- `kubectl`
- `helm` (for cert-manager and Kuadrant)
- `istioctl` (for Istio installation)

## Quick start

```bash
# Clone with submodules / place source dependencies in example.src/
# (kind fork, opendatahub-operator, odh-dashboard)

# Build everything and bring up the cluster
make all

# Install the gateway stack (Istio + cert-manager + Kuadrant)
make gateway-stack

# Build and deploy the ODH operator (includes DSCI, DSC, RBAC)
make operator-install

# Create a test workbench
make workbench

# Log in via CLI (default users defined in users.yaml)
oc login --username=admin --password=admin

# Open the dashboard (browser login form)
# https://rh-ai.apps.ocp-sim.test/

# Optional: enable Models-as-a-Service
make deploy-maas
make dsc-enable-maas
```

`make all` builds the kind fork (with ocp-shim), the node image, the simulator
container, creates the cluster, installs CRDs and seed resources, and deploys
the simulator. Run `make help` for a full list of targets.

### Iterating

```bash
# Rebuild simulator + redeploy without recreating the cluster
make rebuild

# Rebuild just the simulator image and restart the pod
make deploy-sim

# Hot-patch ocp-shim into the running node (no cluster recreate)
make shim-hotpatch

# Tear everything down
make teardown
```

### Workbench management

```bash
# Create a project + workbench (defaults: project1/workbench1)
make workbench

# Custom names and image
make workbench WORKBENCH_PROJECT=myproj WORKBENCH_NAME=wb1 WORKBENCH_IMAGE=jupyter-datascience-notebook:3.4
```

## Project layout

```
picoshift/
├── crds/                 # OpenShift, OLM, Gateway API, Istio, monitoring CRDs
├── seed/                 # Cluster seed resources (ClusterVersion, SCCs, etc.)
├── simulator/            # Rust project — the ocp-sim binary
│   └── src/
│       ├── main.rs
│       ├── route.rs         # Route admission controller
│       ├── gateway.rs       # Gateway API → Envoy xDS (built-in, optional)
│       ├── oauth.rs         # OAuth2 token server
│       ├── project.rs       # Namespace → Project sync
│       ├── service_ca.rs    # Service CA certificate injection
│       ├── pod_mutate.rs    # SCC-like mutating webhook + namespace UID ranges
│       ├── jobset.rs        # JobSet mock controller
│       ├── imagestream.rs   # ImageStream import controller
│       ├── loadbalancer.rs  # LoadBalancer IP assignment for kind
│       └── proxy.rs         # Reverse proxy for Route + HTTPRoute traffic
├── users.yaml            # Default OAuth users (admin, user1, developer)
├── deploy/               # Kubernetes manifests for the simulator
├── scripts/              # Python/bash helper scripts
│   ├── create-workbench.py        # Create a project + workbench (replicates dashboard flow)
│   ├── patch-gatewayconfig-tls.py # Disable IDP cert verification for self-signed CA
│   ├── setup-admin-rbac.py        # Grant admin user cluster permissions
│   └── rebuild.sh                 # Full teardown + rebuild
├── bugs.odh/             # Documented upstream ODH bugs found during testing
├── tasks/                # Roadmap docs for partial → full simulation
├── kind/                 # kind cluster config
└── Makefile
```

`example.src/` (git-ignored) holds cloned dependencies: a kind fork with the
ocp-shim, the opendatahub-operator source, and optionally the ODH Dashboard.

## How it works

1. A kind cluster starts with a custom node image that includes **ocp-shim** —
   a reverse proxy sitting between the API server and clients. It intercepts
   OpenShift-specific discovery requests so tools like `oc` and operators see
   an "OpenShift" cluster.

2. OpenShift CRDs are installed so the API server accepts resources like
   Routes, Projects, and ClusterVersions. Seed resources provide the minimal
   state operators expect at startup (cluster version, infrastructure config,
   default SCCs).

3. **ocp-sim** deploys as a DaemonSet on the control plane node. Its
   controllers watch for Routes, Namespaces, ImageStreams, Services, and
   JobSets, providing the dynamic behavior that operators depend on: Route
   admission, TLS certificates, Project objects, ImageStream status resolution,
   SCC-like pod mutation (fsGroup injection), LoadBalancer IP assignment,
   and JobSet child Job management.

4. **Istio + Kuadrant** provide the real Gateway API data plane. Istio runs
   with the `openshift-gateway` revision so it matches what OSSM/Sail does on
   real OpenShift. Kuadrant provides Authorino and Limitador for auth policies
   and rate limiting.

5. **kube-auth-proxy** (oauth2-proxy) sits alongside the Envoy gateway in
   `openshift-ingress`. An EnvoyFilter wires Envoy's ext_authz to
   kube-auth-proxy, which checks browser sessions. Unauthenticated requests
   get redirected to the **ocp-sim OAuth server**, which presents a login form
   (or responds to `oc login` Basic Auth challenges). After login, the OAuth
   server creates `User` and `Identity` API objects and issues an auth code
   that kube-auth-proxy exchanges for a token and stores in a session cookie.

6. With the simulated control plane in place, the ODH operator starts, detects
   "OpenShift", and reconciles normally. The Dashboard gets a working Gateway
   with OAuth, WebSocket support, and TLS — enough to develop and test against
   without a real OCP cluster.

## What's running

Once fully deployed, the cluster runs the following workloads:

| Namespace | Component | Description |
|-----------|-----------|-------------|
| `ocp-sim` | ocp-sim DaemonSet | All simulated OCP controllers + reverse proxy |
| `istio-system` | istiod (openshift-gateway) | Istio control plane, manages Envoy gateways |
| `cert-manager` | cert-manager | TLS certificate lifecycle management |
| `kuadrant-system` | Kuadrant operator | Authorino + Limitador for auth policies and rate limiting |
| `openshift-ingress` | data-science-gateway (Envoy) | Istio-managed gateway with ext_authz EnvoyFilter, TLS termination |
| `openshift-ingress` | kube-auth-proxy (×2) | oauth2-proxy — manages browser sessions, redirects to OAuth server for login |
| `opendatahub-operator-system` | ODH operator | Reconciles DSC/DSCI into component deployments |
| `opendatahub` | odh-dashboard (×2) | Dashboard UI (9 sidecar containers per pod) |
| `opendatahub` | notebook-controller | Upstream Kubeflow notebook controller |
| `opendatahub` | odh-notebook-controller | ODH notebook controller (webhook, RBAC, HTTPRoute) |
| `opendatahub` | kserve, kuberay, feast, trustyai, etc. | Model serving and ML platform operators |
| `opendatahub` | data-science-pipelines-operator | Pipeline orchestration |
| `opendatahub` | maas-api, maas-controller | Models-as-a-Service (optional, via `make deploy-maas`) |
| `opendatahub` | maas-postgres | MaaS backing database (optional) |
| `odh-model-registries` | model-catalog + postgres | Model registry with backing database |
| `project1` | workbench1 (StatefulSet) | Jupyter notebook + kube-rbac-proxy sidecar |

## Status

Early stage. The opendatahub-operator and ODH Dashboard both start and function
against picoshift. Jupyter workbenches can be created through the dashboard or
CLI, with full OAuth login, kube-rbac-proxy sidecar injection, and WebSocket
support for kernel connections. Coverage of OpenShift APIs will expand as more
operators are tested.

## License

Apache-2.0
