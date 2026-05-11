# maas-api-key-cleanup CronJob uses hardcoded registry.redhat.io image

## Summary

The MaaS (Models as a Service) component deploys a CronJob called
`maas-api-key-cleanup` that runs every 15 minutes. The CronJob container image
is rendered from the `RELATED_IMAGE_UBI_MINIMAL_IMAGE` environment variable on
the maas-controller deployment. When this env var is empty or unset, it defaults
to `registry.redhat.io/ubi9/ubi-minimal:9.7`, which requires Red Hat registry
authentication and fails with `ImagePullBackOff` on clusters without a pull
secret.

## Reproduction

1. Deploy the ODH operator on a cluster without Red Hat registry pull secrets
   (e.g., kind, k3s, vanilla Kubernetes).
2. Enable MaaS via DSC (`kserve.modelsAsService.managementState: Managed`).
3. Wait for the `maas-api-key-cleanup` CronJob to fire (every 15 minutes).
4. The job pod fails with `ImagePullBackOff` because it cannot pull from
   `registry.redhat.io`.

## Root cause

In `internal/controller/components/modelsasservice/modelsasservice_support.go`,
the image map entry for the cleanup job is:

```go
"maas-api-key-cleanup-image": "RELATED_IMAGE_UBI_MINIMAL_IMAGE",
```

This maps to the `RELATED_IMAGE_UBI_MINIMAL_IMAGE` env var on the operator
manager deployment. On OLM-managed installations the CSV pre-populates this
env var with the correct (authenticated) image reference. On non-OLM installs
(dev, picoshift, plain `make deploy`), the env var is absent and the image
reference falls through to the hardcoded default in the manifests:
`registry.redhat.io/ubi9/ubi-minimal:9.7`.

The `registry.redhat.io` registry requires authentication via a Red Hat pull
secret. Clusters that lack this secret (all non-OpenShift clusters, dev
environments) cannot pull the image.

## Expected behavior

The operator should either:
1. Default to a publicly accessible equivalent image
   (`registry.access.redhat.com/ubi9/ubi-minimal:9.7`) when
   `RELATED_IMAGE_UBI_MINIMAL_IMAGE` is unset, or
2. Document the requirement to set this env var for non-OLM installations.

## Workaround

The image reference flows through a `maas-parameters` ConfigMap → `valueFrom`
on the maas-controller deployment. The correct fix is to change the source
`params.env` file:

```
# opt/manifests/maas/overlays/odh/params.env
maas-api-key-cleanup-image=registry.access.redhat.com/ubi9/ubi-minimal:9.7
```

**Do NOT** use `kubectl set env` on the operator deployment to override
`RELATED_IMAGE_UBI_MINIMAL_IMAGE`. The maas-controller manifest uses `valueFrom`
(configMapKeyRef) for this env var. If the operator also injects a `value` field
(from its own env), Kubernetes rejects the deployment with:

```
spec.template.spec.containers[0].env[7].valueFrom: Invalid value: "":
may not be specified when `value` is not empty
```

If the CronJob has already been created with the wrong image, patch it directly:

```sh
kubectl -n opendatahub patch cronjob maas-api-key-cleanup --type=json \
  -p='[{"op":"replace","path":"/spec/jobTemplate/spec/template/spec/containers/0/image","value":"registry.access.redhat.com/ubi9/ubi-minimal:9.7"}]'
```

## Additional observation

Even after fixing the image, the CronJob jobs still show `Failed` status with
`DeadlineExceeded`. The cleanup job runs `curl` against `https://maas-api:8443`,
but the `ubi-minimal` image does not include `curl`. The job command is:

```sh
curl -sf -k -X POST https://maas-api:8443/internal/v1/api-keys/cleanup
```

This is a second, independent issue: the container image used for the cleanup
job must include `curl` (or the command needs to use a tool available in
ubi-minimal, such as a static binary sidecar).

## Affected components

- **opendatahub-operator** — does not set a public default for
  `RELATED_IMAGE_UBI_MINIMAL_IMAGE` in non-OLM deployment manifests
- **maas-controller** — renders the CronJob with whatever image the env var
  resolves to (working as designed)

## Environment

Discovered on picoshift (kind + ocp-sim) running ODH operator from main branch.
Applies to any non-OLM deployment without Red Hat registry credentials.
