CLUSTER_NAME   ?= ocp-sim
KIND_FORK_DIR  := deps/kind
KIND_BIN       := $(KIND_FORK_DIR)/bin/kind
K8S_VERSION    ?= v1.33.1
BASE_IMAGE     := kindest/base:ocp-shim
NODE_IMAGE     := localhost/kindest/node:ocp-shim
SIM_IMAGE      := localhost/ocp-sim:latest

# Auth mode: legacy (sha256~ tokens), oidc (JWT tokens), or byoidc (external OIDC)
AUTH_MODE      ?= legacy

# BYOIDC settings (only used when AUTH_MODE=byoidc)
OIDC_ISSUER_URL    ?=
OIDC_CLIENT_ID     ?=
OIDC_CLIENT_SECRET ?=

# Rootful podman — run `sudo make all` (or set SUDO= to disable)
SUDO           ?= sudo

# ──────────────────────────────────────────────
# Top-level targets
# ──────────────────────────────────────────────

.PHONY: all all-byoidc build-all cluster deploy setup teardown status logs \
       operator-install operator-run operator-crds dsci dsc rebuild \
       workbench patch-gatewayconfig-tls setup-admin-rbac \
       deploy-model-serving deploy-fraud-tutorial \
       gateway-stack gateway-api cert-manager istio kuadrant

help:
	@echo "Usage: make <target>"
	@echo ""
	@echo "Build & Cluster Lifecycle:"
	@echo "  all                Build everything and create cluster with simulator"
	@echo "  all-byoidc         Build everything with BYOIDC + Entra mock IDP"
	@echo "  build-all          Build kind CLI, base image, node image, and sim image"
	@echo "  cluster            Create the kind cluster (idempotent)"
	@echo "  cluster-delete     Delete the kind cluster"
	@echo "  teardown           Alias for cluster-delete"
	@echo "  clean              Delete cluster and remove build artifacts"
	@echo "  rebuild            Rebuild sim image and redeploy (preserves cluster)"
	@echo ""
	@echo "Deploy (base platform):"
	@echo "  deploy             Deploy CRDs + seed resources + simulator + RBAC"
	@echo "  deploy-crds        Apply OpenShift/OLM/Gateway/Istio stub CRDs"
	@echo "  deploy-seed        Apply seed resources (namespaces, config, auth, etc.)"
	@echo "  deploy-sim         Build and deploy the simulator DaemonSet"
	@echo "  redeploy           Rebuild and redeploy simulator only"
	@echo "  deploy-maas        Deploy MaaS CRDs + seed (postgres, gateway, tenant)"
	@echo "  deploy-entra-mock  Deploy Entra ID emulator (OIDC provider) inside cluster"
	@echo "  deploy-byoidc      Deploy Entra mock + redeploy simulator in BYOIDC mode"
	@echo "  setup-admin-rbac   Grant admin user cluster-wide permissions"
	@echo ""
	@echo "Gateway Stack (Istio + Kuadrant):"
	@echo "  gateway-stack      Install gateway-api + cert-manager + Istio + Kuadrant"
	@echo "  gateway-api        Install Gateway API CRDs"
	@echo "  cert-manager       Install cert-manager via Helm"
	@echo "  istio              Install Istio with openshift-gateway revision"
	@echo "  kuadrant           Install Kuadrant operator via Helm"
	@echo "  gateway-stack-delete  Uninstall Kuadrant, Istio, and cert-manager"
	@echo ""
	@echo "ODH Operator:"
	@echo "  operator-install   Full operator setup: build, deploy, DSCI, DSC, RBAC"
	@echo "  operator-deploy    Build operator image, load into cluster, install CRDs"
	@echo "  operator-redeploy  Rebuild and restart the operator"
	@echo "  operator-crds      Generate and install operator CRDs"
	@echo "  operator-run       Run operator locally (outside cluster)"
	@echo "  operator-logs      Tail operator logs"
	@echo "  dsci               Create DSCInitialization + patch GatewayConfig TLS"
	@echo "  dsc                Create DataScienceCluster"
	@echo "  dsc-enable-maas    Enable Models-as-a-Service in the DSC"
	@echo "  patch-authorino-ca Inject Service CA into Authorino for MaaS API trust"
	@echo ""
	@echo "Workbench & Model Serving:"
	@echo "  workbench          Create a workbench (WORKBENCH_PROJECT, WORKBENCH_NAME, WORKBENCH_IMAGE)"
	@echo "  deploy-model-serving  Deploy SeaweedFS + sklearn model serving"
	@echo ""
	@echo "Operate:"
	@echo "  status             Show cluster, node, simulator, and CRD status"
	@echo "  logs               Tail simulator logs"
	@echo "  verify             Run verification checks"
	@echo ""
	@echo "Kind Images:"
	@echo "  init-deps          Clone/update required repos into deps/"
	@echo "  kind-cli           Build the kind CLI binary"
	@echo "  kind-base-image    Build the base image (includes ocp-shim)"
	@echo "  kind-node-image    Build the node image"
	@echo "  sim-image          Build the simulator container image"
	@echo "  shim-hotpatch      Rebuild and replace ocp-shim binary in running cluster"
	@echo ""
	@echo "CLI:"
	@echo "  build-cli          Build the picoshift CLI binary to bin/picoshift"

all: build-all cluster deploy
	@echo ""
	@echo "=== Ready ==="
	@echo "  kubectl get --raw /.well-known/oauth-authorization-server | jq ."
	@echo "  make status"
	@echo "  make logs"
	@echo "  https://rh-ai.apps.ocp-sim.test/"

all-byoidc: build-all cluster deploy-crds
	$(MAKE) deploy-seed AUTH_MODE=byoidc
	$(MAKE) deploy-byoidc
	$(MAKE) setup-admin-rbac
	@echo ""
	@echo "=== Ready (BYOIDC + Entra Mock) ==="
	@echo "  kubectl -n entra-mock get pods"
	@echo "  oc login https://localhost:6443 -u admin -p admin --insecure-skip-tls-verify"
	@echo "  make status"
	@echo "  make logs"

rebuild:
	bash scripts/rebuild.sh

init-deps:
	bash scripts/init-deps.sh

build-all: kind-cli kind-base-image kind-node-image sim-image

build-cli:
	cd cli && go build -o ../bin/picoshift .

# ──────────────────────────────────────────────
# Kind fork (OCP shim)
# ──────────────────────────────────────────────

.PHONY: kind-cli kind-base-image kind-node-image shim-hotpatch

kind-cli: $(KIND_BIN)

$(KIND_BIN):
	$(MAKE) -C $(KIND_FORK_DIR) build

kind-base-image:
	cp $(KIND_FORK_DIR)/cmd/ocp-shim/main.go $(KIND_FORK_DIR)/cmd/ocp-shim/go.mod $(KIND_FORK_DIR)/cmd/ocp-shim/go.sum $(KIND_FORK_DIR)/images/base/ocp-shim/
	$(SUDO) podman build --build-arg GO_VERSION=1.26.2 -t $(BASE_IMAGE) $(KIND_FORK_DIR)/images/base/

kind-node-image: kind-cli kind-base-image
	$(SUDO) $(KIND_BIN) build node-image $(K8S_VERSION) \
		--type release \
		--base-image $(BASE_IMAGE) \
		--image $(NODE_IMAGE)

OCP_SHIM_DIR   := $(KIND_FORK_DIR)/cmd/ocp-shim

shim-hotpatch:
	cd $(OCP_SHIM_DIR) && CGO_ENABLED=0 GOOS=linux GOARCH=amd64 go build -o /tmp/ocp-shim .
	$(SUDO) podman cp /tmp/ocp-shim $(CLUSTER_NAME)-control-plane:/usr/local/bin/ocp-shim
	$(SUDO) podman exec $(CLUSTER_NAME)-control-plane pkill -f ocp-shim
	@rm -f /tmp/ocp-shim
	@sleep 2
	@$(SUDO) podman exec $(CLUSTER_NAME)-control-plane pgrep -a ocp-shim \
		&& echo "ocp-shim restarted" || echo "WARNING: ocp-shim did not restart"

# ──────────────────────────────────────────────
# Simulator image
# ──────────────────────────────────────────────

.PHONY: sim-image sim-load sim-build

sim-build:
	cd simulator && cargo build --release --target x86_64-unknown-linux-gnu

sim-image:
	$(SUDO) podman build -t $(SIM_IMAGE) ./simulator

sim-load:
	$(SUDO) podman save $(SIM_IMAGE) --format oci-archive -o /tmp/ocp-sim-oci.tar
	$(SUDO) podman exec -i $(CLUSTER_NAME)-control-plane \
		ctr --namespace=k8s.io images import --no-unpack - < /tmp/ocp-sim-oci.tar
	$(SUDO) rm -f /tmp/ocp-sim-oci.tar

# ──────────────────────────────────────────────
# Cluster lifecycle
# ──────────────────────────────────────────────

.PHONY: cluster cluster-delete

cluster: kind-cli
	@if $(SUDO) $(KIND_BIN) get clusters 2>/dev/null | grep -q "^$(CLUSTER_NAME)$$"; then \
		echo "cluster '$(CLUSTER_NAME)' already exists"; \
	else \
		$(SUDO) $(KIND_BIN) create cluster \
			--config deploy/kind/cluster.yaml \
			--image $(NODE_IMAGE) \
			--name $(CLUSTER_NAME); \
		$(SUDO) $(KIND_BIN) get kubeconfig --name $(CLUSTER_NAME) > $(shell echo ~$(shell id -un))/.kube/config; \
	fi

cluster-delete:
	$(SUDO) $(KIND_BIN) delete cluster --name $(CLUSTER_NAME)

# ──────────────────────────────────────────────
# Deploy (CRDs + seed + simulator)
# ──────────────────────────────────────────────

.PHONY: deploy deploy-crds deploy-seed deploy-sim deploy-maas deploy-entra-mock deploy-byoidc redeploy

deploy: deploy-crds deploy-seed deploy-sim setup-admin-rbac

deploy-crds:
	@echo "Waiting for API server..."
	@kubectl wait --for=condition=Ready node --all --timeout=120s
	kubectl apply -f deploy/crds/openshift/
	kubectl apply -f deploy/crds/olm/
	kubectl apply -f deploy/crds/gateway/
	kubectl apply -f deploy/crds/monitoring/
	kubectl apply -f deploy/crds/istio/
	kubectl apply --server-side -f deploy/crds/jobset/
	kubectl apply -f deploy/crds/authorino/
	kubectl apply -f deploy/crds/kuadrant/
	kubectl wait --for=condition=Established crd --all --timeout=30s

deploy-seed: deploy-seed-resources deploy-clusterversion deploy-endpoint-patch

deploy-seed-resources:
	kubectl apply -f deploy/seed/namespaces.yaml
	kubectl apply -f deploy/seed/cluster-config.yaml
ifeq ($(AUTH_MODE),legacy)
	kubectl apply -f deploy/seed/authentication.yaml
else
	kubectl apply -f deploy/seed/authentication.yaml
	kubectl apply -f deploy/seed/authentication-oidc.yaml
endif
	kubectl apply -f deploy/seed/ingress.yaml
	kubectl apply -f deploy/seed/infrastructure.yaml
	kubectl apply -f deploy/seed/sccs.yaml
	kubectl apply -f deploy/seed/jobset-operator.yaml
	kubectl apply -f deploy/seed/rbac-compat.yaml

deploy-endpoint-patch:
	@# Route in-cluster kubernetes service traffic through the ocp-shim (port 6443)
	@# so that /.well-known/oauth-authorization-server is accessible anonymously.
	kubectl patch endpoints kubernetes -n default --type='json' \
		-p='[{"op":"replace","path":"/subsets/0/ports/0/port","value":6443}]' 2>/dev/null || true

deploy-clusterversion:
	@# Create the spec-only object, then PUT status via the status subresource
	kubectl apply -f - <<< '{"apiVersion":"config.openshift.io/v1","kind":"ClusterVersion","metadata":{"name":"version"},"spec":{"clusterID":"ocp-sim-00000000-0000-0000-0000-000000000000","channel":"stable-4.20"}}'
	@kubectl proxy --port=8199 & PROXY_PID=$$!; \
	sleep 1; \
	RV=$$(curl -s http://localhost:8199/apis/config.openshift.io/v1/clusterversions/version | python3 -c "import sys,json; print(json.load(sys.stdin)['metadata']['resourceVersion'])"); \
	curl -s -X PUT http://localhost:8199/apis/config.openshift.io/v1/clusterversions/version/status \
		-H "Content-Type: application/json" \
		-d "{\"apiVersion\":\"config.openshift.io/v1\",\"kind\":\"ClusterVersion\",\"metadata\":{\"name\":\"version\",\"resourceVersion\":\"$$RV\"},\"spec\":{\"clusterID\":\"ocp-sim-00000000-0000-0000-0000-000000000000\",\"channel\":\"stable-4.20\"},\"status\":{\"desired\":{\"version\":\"4.20.0\"},\"history\":[{\"state\":\"Completed\",\"version\":\"4.20.0\",\"startedTime\":\"2024-01-01T00:00:00Z\",\"completionTime\":\"2024-01-01T01:00:00Z\",\"verified\":true}],\"conditions\":[{\"type\":\"Available\",\"status\":\"True\",\"lastTransitionTime\":\"2024-01-01T01:00:00Z\",\"reason\":\"ClusterVersionAvailable\",\"message\":\"Simulated OCP cluster\"},{\"type\":\"Progressing\",\"status\":\"False\",\"lastTransitionTime\":\"2024-01-01T01:00:00Z\",\"reason\":\"ClusterVersionNotProgressing\"},{\"type\":\"Failing\",\"status\":\"False\",\"lastTransitionTime\":\"2024-01-01T01:00:00Z\",\"reason\":\"ClusterVersionNotFailing\"}]}}" > /dev/null; \
	kill $$PROXY_PID 2>/dev/null; wait $$PROXY_PID 2>/dev/null || true
	@echo "ClusterVersion 'version' created with status"

deploy-sim: sim-image sim-load
	kubectl wait --for=condition=Ready node --all --timeout=120s
	kubectl create namespace ocp-sim 2>/dev/null || true
	kubectl -n ocp-sim create configmap ocp-sim-users --from-file=deploy/users.yaml --dry-run=client -o yaml | kubectl apply -f -
	kubectl apply -f deploy/simulator.yaml
ifeq ($(AUTH_MODE),oidc)
	kubectl -n ocp-sim patch daemonset ocp-sim --type=json \
		-p='[{"op":"replace","path":"/spec/template/spec/containers/0/args","value":["--proxy","--proxy-port","80","--users-file","/etc/ocp-sim/users.yaml","--auth-mode","oidc"]}]'
endif
ifeq ($(AUTH_MODE),byoidc)
	kubectl -n ocp-sim patch daemonset ocp-sim --type=json \
		-p='[{"op":"replace","path":"/spec/template/spec/containers/0/args","value":["--proxy","--proxy-port","80","--users-file","/etc/ocp-sim/users.yaml","--auth-mode","byoidc","--oidc-issuer-url","$(OIDC_ISSUER_URL)","--oidc-client-id","$(OIDC_CLIENT_ID)","--oidc-client-secret","$(OIDC_CLIENT_SECRET)"]}]'
endif
	kubectl -n ocp-sim delete pod --all --wait=false 2>/dev/null || true
	@sleep 3
	kubectl wait --namespace ocp-sim \
		--for=condition=Ready pod \
		--selector=app=ocp-sim \
		--timeout=120s

deploy-maas:
	kubectl apply -f deploy/crds/maas/
	kubectl wait --for=condition=Established crd --all --timeout=30s
	kubectl apply -f deploy/seed/maas.yaml

deploy-entra-mock:
	bash scripts/deploy-entra-mock.sh

BYOIDC_TENANT  ?= a1b2c3d4-e5f6-7890-abcd-ef1234567890
BYOIDC_ISSUER  := https://entra.apps.ocp-sim.test/$(BYOIDC_TENANT)/v2.0

deploy-byoidc: deploy-entra-mock
	$(MAKE) deploy-sim AUTH_MODE=byoidc \
		OIDC_ISSUER_URL=http://entra-mock.entra-mock.svc.cluster.local:8080/$(BYOIDC_TENANT)/v2.0 \
		OIDC_CLIENT_ID=picoshift \
		OIDC_CLIENT_SECRET=picoshift-secret
	$(MAKE) patch-apiserver-oidc
	@# Patch GatewayConfig with OIDC settings if it exists
	@if kubectl get gatewayconfig default-gateway >/dev/null 2>&1; then \
		kubectl create secret generic oidc-client-secret \
			--from-literal=client-secret=picoshift-secret \
			-n openshift-ingress --dry-run=client -o yaml | kubectl apply -f -; \
		kubectl patch gatewayconfig default-gateway --type=merge \
			-p '{"spec":{"oidc":{"issuerURL":"$(BYOIDC_ISSUER)","clientID":"picoshift","clientSecretRef":{"name":"oidc-client-secret","key":"client-secret"}}}}'; \
	fi

patch-apiserver-oidc:
	@echo "Extracting OIDC CA cert from simulator proxy..."
	@$(SUDO) podman exec $(CLUSTER_NAME)-control-plane sh -c \
		'echo | openssl s_client -connect localhost:443 -servername entra.apps.ocp-sim.test 2>/dev/null \
		| openssl x509 -outform PEM > /etc/kubernetes/pki/oidc-ca.crt'
	@echo "Patching kube-apiserver with OIDC flags..."
	@$(SUDO) podman exec $(CLUSTER_NAME)-control-plane sh -c \
		'grep -q -- "--oidc-issuer-url=https://entra" /etc/kubernetes/manifests/kube-apiserver.yaml && exit 0; \
		sed -i "/--secure-port=16443/a\\        - --oidc-issuer-url=$(BYOIDC_ISSUER)\n        - --oidc-client-id=picoshift\n        - --oidc-username-claim=preferred_username\n        - --oidc-groups-claim=groups\n        - --oidc-username-prefix=-\n        - --oidc-groups-prefix=\n        - --oidc-ca-file=/etc/kubernetes/pki/oidc-ca.crt" \
		/etc/kubernetes/manifests/kube-apiserver.yaml'
	@echo "Patching ocp-shim with OIDC issuer URL..."
	@$(SUDO) podman exec $(CLUSTER_NAME)-control-plane sh -c \
		'sed -i "s|--oidc-issuer-url=https://localhost:443|--oidc-issuer-url=https://localhost:9443|" \
		/etc/kubernetes/manifests/kube-apiserver.yaml'
	@echo "Waiting for kube-apiserver to restart with OIDC..."
	@sleep 20
	@kubectl get nodes --request-timeout=60s >/dev/null 2>&1 || sleep 10
	@kubectl get nodes --request-timeout=60s
	@echo "kube-apiserver OIDC configuration complete"

redeploy: deploy-sim

# ──────────────────────────────────────────────
# Operate
# ──────────────────────────────────────────────

.PHONY: logs status verify

logs:
	kubectl -n ocp-sim logs -l app=ocp-sim -f

status:
	@echo "=== Cluster ==="
	@$(SUDO) $(KIND_BIN) get clusters 2>/dev/null | grep -q "^$(CLUSTER_NAME)$$" \
		&& echo "kind cluster '$(CLUSTER_NAME)': running" \
		|| echo "kind cluster '$(CLUSTER_NAME)': not found"
	@echo ""
	@echo "=== Nodes ==="
	@kubectl get nodes 2>/dev/null || echo "cluster not reachable"
	@echo ""
	@echo "=== Simulator ==="
	@kubectl -n ocp-sim get pods 2>/dev/null || echo "not deployed"
	@echo ""
	@echo "=== OCP Shim ==="
	@kubectl get --raw /.well-known/oauth-authorization-server 2>/dev/null | python3 -m json.tool \
		|| echo "well-known endpoint not available"
	@echo ""
	@echo "=== OAuth Server ==="
	@$(SUDO) podman exec $(CLUSTER_NAME)-control-plane \
		curl -sk https://localhost:443/.well-known/oauth-authorization-server 2>/dev/null | python3 -m json.tool \
		|| echo "oauth server not reachable"
	@echo ""
	@echo "=== OpenShift CRDs ==="
	@kubectl get crd -o name 2>/dev/null | grep -c "openshift.io\|operators.coreos.com" | xargs -I{} echo "{} CRDs installed" \
		|| echo "cluster not reachable"

verify:
	@echo "--- API server well-known ---"
	kubectl get --raw /.well-known/oauth-authorization-server | python3 -m json.tool
	@echo ""
	@echo "--- OAuth server (via node) ---"
	$(SUDO) podman exec $(CLUSTER_NAME)-control-plane curl -sk https://localhost:443/.well-known/oauth-authorization-server | python3 -m json.tool
	@echo ""
	@echo "--- Nodes ---"
	kubectl get nodes
	@echo ""
	@echo "--- Simulator pod ---"
	kubectl -n ocp-sim get pods

# ──────────────────────────────────────────────
# ODH Operator
# ──────────────────────────────────────────────

ODH_DIR            := deps/opendatahub-operator
ODH_OPERATOR_IMAGE := localhost/odh-operator:latest
ODH_KUSTOMIZE      := $(ODH_DIR)/bin/kustomize
OPERATOR_NAMESPACE := opendatahub-operator-system

.PHONY: operator-crds operator-run operator-image operator-load \
        operator-deploy operator-redeploy operator-logs \
        dsci dsc dsc-enable-maas patch-authorino-ca operator-install

operator-crds:
	$(MAKE) -C $(ODH_DIR) manifests
	$(MAKE) -C $(ODH_DIR) install

operator-run:
	ODH_MANAGER_METRICS_BIND_ADDRESS=:9090 $(MAKE) -C $(ODH_DIR) run-nowebhook

operator-image:
	$(MAKE) -C $(ODH_DIR) manifests
	$(SUDO) podman build --no-cache \
		-f $(ODH_DIR)/Dockerfiles/Dockerfile \
		--build-arg USE_LOCAL=false \
		--build-arg CGO_ENABLED=1 \
		--platform linux/amd64 \
		-t $(ODH_OPERATOR_IMAGE) \
		$(ODH_DIR)

operator-load:
	$(SUDO) podman save $(ODH_OPERATOR_IMAGE) --format oci-archive -o /tmp/odh-operator-oci.tar
	$(SUDO) podman exec -i $(CLUSTER_NAME)-control-plane \
		ctr --namespace=k8s.io images import --no-unpack - < /tmp/odh-operator-oci.tar
	$(SUDO) rm -f /tmp/odh-operator-oci.tar

operator-deploy: operator-image operator-load operator-crds
	$(MAKE) -C $(ODH_DIR) prepare IMG=$(ODH_OPERATOR_IMAGE)
	$(ODH_KUSTOMIZE) build $(ODH_DIR)/config/default \
		| sed 's/imagePullPolicy: Always/imagePullPolicy: IfNotPresent/g' \
		| sed 's/replicas: 3/replicas: 1/g' \
		| kubectl apply -f -
	kubectl -n $(OPERATOR_NAMESPACE) rollout status deployment/opendatahub-operator-controller-manager --timeout=120s

operator-redeploy:
	$(MAKE) operator-image
	$(MAKE) operator-load
	kubectl -n $(OPERATOR_NAMESPACE) rollout restart deployment/opendatahub-operator-controller-manager
	kubectl -n $(OPERATOR_NAMESPACE) rollout status deployment/opendatahub-operator-controller-manager --timeout=120s

operator-logs:
	kubectl -n $(OPERATOR_NAMESPACE) logs -l control-plane=controller-manager -c manager -f

operator-install: operator-deploy dsci dsc setup-admin-rbac

dsci:
	kubectl apply -f $(ODH_DIR)/config/samples/dscinitialization_v2_dscinitialization.yaml
	@# Disable TLS verification for the OIDC provider (self-signed certs on picoshift)
	@echo "Waiting for GatewayConfig to be created..."
	@for i in $$(seq 1 30); do \
		kubectl get gatewayconfig default-gateway >/dev/null 2>&1 && break; \
		sleep 2; \
	done
	kubectl patch gatewayconfig default-gateway --type=merge \
		-p '{"spec":{"verifyProviderCertificate":false}}'

dsc:
	kubectl apply -f deploy/seed/datasciencecluster.yaml

dsc-enable-maas:
	kubectl patch datasciencecluster default-dsc --type=merge \
		-p '{"spec":{"components":{"kserve":{"modelsAsService":{"managementState":"Managed"}}}}}'

patch-authorino-ca:
	@# Ensure odh-trusted-ca-bundle ConfigMap exists in kuadrant-system with
	@# the inject label so the Service CA controller populates it.
	kubectl apply -f - <<< '{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"odh-trusted-ca-bundle","namespace":"kuadrant-system","labels":{"config.openshift.io/inject-trusted-cabundle":"true","app.kubernetes.io/part-of":"opendatahub-operator"}}}'
	@echo "Waiting for Service CA to inject into odh-trusted-ca-bundle..."
	@for i in $$(seq 1 15); do \
		LEN=$$(kubectl get cm odh-trusted-ca-bundle -n kuadrant-system -o jsonpath='{.data.odh-ca-bundle\.crt}' 2>/dev/null | wc -c); \
		if [ "$$LEN" -gt 10 ]; then echo "CA cert injected ($$LEN bytes)"; break; fi; \
		sleep 2; \
	done
	@# Patch Authorino CR to mount the CA bundle (no subPath — auto-updates)
	kubectl patch authorino authorino -n kuadrant-system --type=merge \
		-p '{"spec":{"volumes":{"items":[{"name":"odh-ca","mountPath":"/etc/ssl/custom-certs","configMaps":["odh-trusted-ca-bundle"]}]}}}'
	@# Set SSL_CERT_FILE so Go's x509 package trusts our Service CA
	kubectl set env deployment/authorino -n kuadrant-system \
		SSL_CERT_FILE=/etc/ssl/custom-certs/odh-ca-bundle.crt
	kubectl rollout status deployment/authorino -n kuadrant-system --timeout=60s

# ──────────────────────────────────────────────
# Workbench
# ──────────────────────────────────────────────

WORKBENCH_PROJECT   ?= project1
WORKBENCH_NAME      ?= workbench1
WORKBENCH_IMAGE     ?= jupyter-minimal-notebook:3.4

.PHONY: workbench

workbench:
	python3 scripts/create-workbench.py \
		--project $(WORKBENCH_PROJECT) \
		--workbench $(WORKBENCH_NAME) \
		--image $(WORKBENCH_IMAGE)

patch-gatewayconfig-tls:
	python3 scripts/patch-gatewayconfig-tls.py

setup-admin-rbac:
	python3 scripts/setup-admin-rbac.py

# ──────────────────────────────────────────────
# Model Serving
# ──────────────────────────────────────────────

.PHONY: deploy-model-serving

deploy-model-serving:
	python3 scripts/deploy-model-serving.py

# ──────────────────────────────────────────────
# Gateway Stack (Istio + Kuadrant)
# ──────────────────────────────────────────────

GATEWAY_API_VERSION ?= v1.3.0

.PHONY: gateway-stack gateway-api cert-manager istio kuadrant \
        gateway-stack-delete cert-manager-delete istio-delete kuadrant-delete

gateway-stack: gateway-api cert-manager istio kuadrant
	@echo "Gateway stack ready (cert-manager + Istio + Kuadrant)"

gateway-api:
	kubectl apply -f https://github.com/kubernetes-sigs/gateway-api/releases/download/$(GATEWAY_API_VERSION)/standard-install.yaml

cert-manager:
	helm repo add jetstack https://charts.jetstack.io --force-update
	helm upgrade --install cert-manager jetstack/cert-manager \
		--namespace cert-manager --create-namespace \
		--set crds.enabled=true \
		--wait --timeout 120s

istio:
	@# Install Istio with the "openshift-gateway" revision and OpenShift controller
	@# name so it behaves like OSSM/Sail on real OpenShift:
	@# - revision=openshift-gateway: matches istio.io/rev label set by ODH operator
	@# - PILOT_GATEWAY_API_CONTROLLER_NAME: watches the same controller name ODH uses
	@# - PILOT_ENABLE_GATEWAY_API_DEPLOYMENT_CONTROLLER: ensures gateway Deployment/Service creation
	istioctl install --set profile=minimal \
		--set revision=openshift-gateway \
		--set values.pilot.resources.requests.cpu=100m \
		--set values.pilot.resources.requests.memory=256Mi \
		--set values.pilot.env.PILOT_GATEWAY_API_CONTROLLER_NAME="openshift.io/gateway-controller/v1" \
		--set values.pilot.env.PILOT_ENABLE_GATEWAY_API_DEPLOYMENT_CONTROLLER=true \
		-y

kuadrant:
	@# Remove stub CRDs that conflict with Kuadrant's own CRDs
	-kubectl delete crd authconfigs.authorino.kuadrant.io 2>/dev/null || true
	-kubectl delete crd authorinos.operator.authorino.kuadrant.io 2>/dev/null || true
	-kubectl delete crd authpolicies.kuadrant.io 2>/dev/null || true
	-kubectl delete crd tokenratelimitpolicies.kuadrant.io 2>/dev/null || true
	helm repo add kuadrant https://kuadrant.io/helm-charts/ --force-update
	helm upgrade --install kuadrant kuadrant/kuadrant-operator \
		--namespace kuadrant-system --create-namespace \
		--wait --timeout 180s
	kubectl set env deployment/kuadrant-operator-controller-manager \
		-n kuadrant-system \
		ISTIO_GATEWAY_CONTROLLER_NAMES="openshift.io/gateway-controller/v1"
	kubectl rollout status deployment/kuadrant-operator-controller-manager \
		-n kuadrant-system --timeout=60s
	kubectl apply -f - <<< '{"apiVersion":"kuadrant.io/v1beta1","kind":"Kuadrant","metadata":{"name":"kuadrant","namespace":"kuadrant-system"},"spec":{}}'
	@echo "Waiting for Kuadrant to become ready..."
	@for i in $$(seq 1 30); do \
		STATUS=$$(kubectl get kuadrant kuadrant -n kuadrant-system -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}' 2>/dev/null); \
		if [ "$$STATUS" = "True" ]; then echo "Kuadrant is ready"; break; fi; \
		sleep 5; \
	done

gateway-stack-delete: kuadrant-delete istio-delete cert-manager-delete

kuadrant-delete:
	-helm uninstall kuadrant -n kuadrant-system 2>/dev/null || true
	-kubectl delete namespace kuadrant-system 2>/dev/null || true

istio-delete:
	-istioctl uninstall --purge -y 2>/dev/null || true
	-kubectl delete namespace istio-system 2>/dev/null || true

cert-manager-delete:
	-helm uninstall cert-manager -n cert-manager 2>/dev/null || true
	-kubectl delete namespace cert-manager 2>/dev/null || true

# ──────────────────────────────────────────────
# Cleanup
# ──────────────────────────────────────────────

.PHONY: teardown clean

teardown: cluster-delete

clean: teardown
	rm -f /tmp/ocp-sim-oci.tar
	cd $(KIND_FORK_DIR) && rm -rf bin/ images/base/ocp-shim/
