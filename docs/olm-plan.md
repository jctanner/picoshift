# Plan: Optional OLM Installation for picoshift

## Context

picoshift currently stubs OLM CRDs (`crds/olm/olm-crds.yaml`) so operators that probe for OLM types don't crash, but no actual OLM controllers run. The ODH operator is deployed via kustomize (`make operator-deploy`). To test RHOAI **upgrades**, we need real OLM — Subscription → CSV → InstallPlan lifecycle — so the upgrade path matches production OCP.

OCP 4.18/4.19 ships **OLM v0 (classic)** with v1 as tech preview only. We'll install OLM v0 (latest upstream: v0.42.0) as an optional add-on, keeping the default kustomize path untouched.

## Approach: Two separate make targets

The existing `make operator-install` (kustomize) stays as-is. New targets add OLM as an opt-in layer:

### New Makefile targets

| Target | What it does |
|--------|-------------|
| `olm-install` | Install OLM v0 onto the cluster (delete stub CRDs first, apply real OLM manifests, wait for rollout) |
| `olm-uninstall` | Remove OLM and restore stub CRDs |
| `operator-install-olm` | Install ODH operator via OLM: create CatalogSource + OperatorGroup + Subscription, wait for CSV |

### Implementation details

#### 1. `olm-install` target in Makefile

```makefile
OLM_VERSION ?= v0.42.0

olm-install:
	@echo "Removing stub OLM CRDs (real OLM brings its own)..."
	-kubectl delete -f crds/olm/ 2>/dev/null || true
	@echo "Installing OLM $(OLM_VERSION)..."
	kubectl create -f https://github.com/operator-framework/operator-lifecycle-manager/releases/download/$(OLM_VERSION)/crds.yaml
	kubectl wait --for=condition=Established crd --all --timeout=30s
	kubectl create -f https://github.com/operator-framework/operator-lifecycle-manager/releases/download/$(OLM_VERSION)/olm.yaml
	kubectl rollout status deployment/olm-operator -n olm --timeout=120s
	kubectl rollout status deployment/catalog-operator -n olm --timeout=120s
	@echo "OLM $(OLM_VERSION) installed"

olm-uninstall:
	-kubectl delete -f https://github.com/operator-framework/operator-lifecycle-manager/releases/download/$(OLM_VERSION)/olm.yaml 2>/dev/null || true
	-kubectl delete -f https://github.com/operator-framework/operator-lifecycle-manager/releases/download/$(OLM_VERSION)/crds.yaml 2>/dev/null || true
	-kubectl delete namespace olm 2>/dev/null || true
	@echo "Restoring stub OLM CRDs..."
	kubectl apply -f crds/olm/
	@echo "OLM uninstalled, stub CRDs restored"
```

Key detail: the stub CRDs in `crds/olm/` conflict with real OLM's CRDs (same group/resource names but minimal schemas). We delete them before installing OLM — same pattern used for Kuadrant (`kuadrant:` target lines 448-451). On uninstall, we restore the stubs so the default non-OLM path keeps working.

#### 2. `operator-install-olm` target in Makefile

Create `deploy/olm/` directory with manifests:

**`deploy/olm/catalogsource.yaml`** — points at the community ODH operator catalog (or a pinned version):
```yaml
apiVersion: operators.coreos.com/v1alpha1
kind: CatalogSource
metadata:
  name: community-operators
  namespace: olm
spec:
  sourceType: grpc
  image: quay.io/operatorhubio/catalog:latest
  displayName: Community Operators
```

**`deploy/olm/operatorgroup.yaml`**:
```yaml
apiVersion: operators.coreos.com/v1
kind: OperatorGroup
metadata:
  name: opendatahub-og
  namespace: openshift-operators
```

**`deploy/olm/subscription.yaml`**:
```yaml
apiVersion: operators.coreos.com/v1alpha1
kind: Subscription
metadata:
  name: opendatahub-operator
  namespace: openshift-operators
spec:
  channel: fast
  name: opendatahub-operator
  source: community-operators
  sourceNamespace: olm
  installPlanApproval: Automatic
```

The Makefile target:
```makefile
operator-install-olm:
	kubectl apply -f deploy/olm/catalogsource.yaml
	@echo "Waiting for CatalogSource to be ready..."
	@for i in $$(seq 1 60); do \
		STATE=$$(kubectl get catalogsource community-operators -n olm -o jsonpath='{.status.connectionState.lastObservedState}' 2>/dev/null); \
		if [ "$$STATE" = "READY" ]; then echo "CatalogSource ready"; break; fi; \
		sleep 5; \
	done
	kubectl apply -f deploy/olm/operatorgroup.yaml
	kubectl apply -f deploy/olm/subscription.yaml
	@echo "Waiting for ODH operator CSV to succeed..."
	@for i in $$(seq 1 60); do \
		PHASE=$$(kubectl get csv -n openshift-operators -l operators.coreos.com/opendatahub-operator.openshift-operators="" -o jsonpath='{.items[0].status.phase}' 2>/dev/null); \
		if [ "$$PHASE" = "Succeeded" ]; then echo "ODH operator installed via OLM"; break; fi; \
		sleep 5; \
	done
	$(MAKE) dsci dsc setup-admin-rbac
```

#### 3. Update `deploy-crds` to be OLM-aware

Add a guard so `deploy-crds` skips the stub OLM CRDs if real OLM is already running:

```makefile
deploy-crds:
	@echo "Waiting for API server..."
	@kubectl wait --for=condition=Ready node --all --timeout=120s
	kubectl apply -f crds/openshift/
	@if kubectl get deployment olm-operator -n olm >/dev/null 2>&1; then \
		echo "OLM is running — skipping stub OLM CRDs"; \
	else \
		kubectl apply -f crds/olm/; \
	fi
	kubectl apply -f crds/gateway/
	# ... rest unchanged
```

#### 4. Update help text and README

- Add `olm-install`, `olm-uninstall`, and `operator-install-olm` to the `help:` target
- Add a section to README explaining the optional OLM path

## Files to create/modify

| File | Action |
|------|--------|
| `Makefile` | Add `olm-install`, `olm-uninstall`, `operator-install-olm` targets; update `deploy-crds` guard; update `help` |
| `deploy/olm/catalogsource.yaml` | **New** — CatalogSource pointing at community catalog |
| `deploy/olm/operatorgroup.yaml` | **New** — OperatorGroup in openshift-operators |
| `deploy/olm/subscription.yaml` | **New** — Subscription for opendatahub-operator |
| `README.md` | Add OLM section under Quick start |

## Verification

1. **Default path still works**: `make all` → `make operator-install` — should skip OLM, use stub CRDs, deploy via kustomize as before
2. **OLM install**: `make olm-install` → verify `kubectl get deploy -n olm` shows `olm-operator`, `catalog-operator`, and `packageserver` all running
3. **OLM operator install**: `make operator-install-olm` → verify CSV in `openshift-operators` namespace reaches `Succeeded` phase, operator pod starts, DSCI/DSC reconcile
4. **OLM uninstall**: `make olm-uninstall` → verify OLM pods gone, stub CRDs restored
5. **Idempotence**: `make deploy-crds` with OLM running should skip stubs; without OLM should apply stubs
