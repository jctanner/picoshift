CLUSTER_NAME   ?= ocp-sim
KIND_FORK_DIR  := example.src/kind
KIND_BIN       := $(KIND_FORK_DIR)/bin/kind
K8S_VERSION    ?= v1.33.1
BASE_IMAGE     := kindest/base:ocp-shim
NODE_IMAGE     := localhost/kindest/node:ocp-shim
SIM_IMAGE      := localhost/ocp-sim:latest

# Rootful podman — run `sudo make all` (or set SUDO= to disable)
SUDO           ?= sudo

# ──────────────────────────────────────────────
# Top-level targets
# ──────────────────────────────────────────────

.PHONY: all build-all cluster deploy setup teardown status logs \
       operator-install operator-run operator-crds dsci dsc rebuild \
       workbench patch-gatewayconfig-tls setup-admin-rbac \
       deploy-model-serving deploy-fraud-tutorial

all: build-all cluster deploy
	@echo ""
	@echo "=== Ready ==="
	@echo "  kubectl get --raw /.well-known/oauth-authorization-server | jq ."
	@echo "  make status"
	@echo "  make logs"
	@echo "  https://rh-ai.apps.ocp-sim.localhost/"

rebuild:
	bash scripts/rebuild.sh

build-all: kind-cli kind-base-image kind-node-image sim-image

# ──────────────────────────────────────────────
# Kind fork (OCP shim)
# ──────────────────────────────────────────────

.PHONY: kind-cli kind-base-image kind-node-image shim-hotpatch

kind-cli: $(KIND_BIN)

$(KIND_BIN):
	$(MAKE) -C $(KIND_FORK_DIR) build

kind-base-image:
	cp $(KIND_FORK_DIR)/cmd/ocp-shim/main.go $(KIND_FORK_DIR)/cmd/ocp-shim/go.mod $(KIND_FORK_DIR)/images/base/ocp-shim/
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
			--config kind/cluster.yaml \
			--image $(NODE_IMAGE) \
			--name $(CLUSTER_NAME); \
		$(SUDO) $(KIND_BIN) get kubeconfig --name $(CLUSTER_NAME) > $(shell echo ~$(shell id -un))/.kube/config; \
	fi

cluster-delete:
	$(SUDO) $(KIND_BIN) delete cluster --name $(CLUSTER_NAME)

# ──────────────────────────────────────────────
# Deploy (CRDs + seed + simulator)
# ──────────────────────────────────────────────

.PHONY: deploy deploy-crds deploy-seed deploy-sim redeploy

deploy: deploy-crds deploy-seed deploy-sim

deploy-crds:
	kubectl apply -f crds/openshift/
	kubectl apply -f crds/olm/
	kubectl apply -f crds/gateway/
	kubectl apply -f crds/monitoring/
	kubectl apply -f crds/istio/
	kubectl apply --server-side -f crds/jobset/
	kubectl apply -f crds/maas/
	kubectl apply -f crds/authorino/
	kubectl apply -f crds/kuadrant/
	kubectl wait --for=condition=Established crd --all --timeout=30s

deploy-seed: deploy-seed-resources deploy-clusterversion deploy-endpoint-patch

deploy-seed-resources:
	kubectl apply -f seed/namespaces.yaml
	kubectl apply -f seed/cluster-config.yaml
	kubectl apply -f seed/authentication.yaml
	kubectl apply -f seed/ingress.yaml
	kubectl apply -f seed/infrastructure.yaml
	kubectl apply -f seed/sccs.yaml
	kubectl apply -f seed/jobset-operator.yaml
	kubectl apply -f seed/rbac-compat.yaml
	kubectl apply -f seed/maas.yaml

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
	kubectl apply -f deploy/simulator.yaml
	kubectl -n ocp-sim delete pod --all --wait=false 2>/dev/null || true
	@sleep 3
	kubectl wait --namespace ocp-sim \
		--for=condition=Ready pod \
		--selector=app=ocp-sim \
		--timeout=120s

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
		curl -sk https://localhost:9443/.well-known/oauth-authorization-server 2>/dev/null | python3 -m json.tool \
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
	$(SUDO) podman exec $(CLUSTER_NAME)-control-plane curl -sk https://localhost:9443/.well-known/oauth-authorization-server | python3 -m json.tool
	@echo ""
	@echo "--- Nodes ---"
	kubectl get nodes
	@echo ""
	@echo "--- Simulator pod ---"
	kubectl -n ocp-sim get pods

# ──────────────────────────────────────────────
# ODH Operator
# ──────────────────────────────────────────────

ODH_DIR            := example.src/opendatahub-operator
ODH_OPERATOR_IMAGE := localhost/odh-operator:latest
ODH_KUSTOMIZE      := $(ODH_DIR)/bin/kustomize
OPERATOR_NAMESPACE := opendatahub-operator-system

.PHONY: operator-crds operator-run operator-image operator-load \
        operator-deploy operator-redeploy operator-logs \
        dsci dsc dsc-enable-maas operator-install

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

operator-install: operator-deploy dsci dsc

dsci:
	kubectl apply -f $(ODH_DIR)/config/samples/dscinitialization_v2_dscinitialization.yaml

dsc:
	kubectl apply -f $(ODH_DIR)/config/samples/datasciencecluster_v2_datasciencecluster.yaml

dsc-enable-maas:
	kubectl patch datasciencecluster default-dsc --type=merge \
		-p '{"spec":{"components":{"kserve":{"modelsAsService":{"managementState":"Managed"}}}}}'

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

deploy-fraud-tutorial:
	cd odh.mcp && .venv/bin/python ../scripts/deploy-fraud-tutorial.py

# ──────────────────────────────────────────────
# Cleanup
# ──────────────────────────────────────────────

.PHONY: teardown clean

teardown: cluster-delete

clean: teardown
	rm -f /tmp/ocp-sim-oci.tar
	cd $(KIND_FORK_DIR) && rm -rf bin/ images/base/ocp-shim/
