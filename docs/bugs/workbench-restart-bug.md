# Dashboard does not set restart annotation on workbench image change

## Summary

When a user edits a workbench in the ODH dashboard and changes the notebook
image, the dashboard updates the Notebook CR spec but does not set the
`notebooks.opendatahub.io/notebook-restart: "true"` annotation. This causes
the pod to remain stuck on the old (possibly broken) image because the
StatefulSet `RollingUpdate` strategy won't replace a pod that was never ready.

## Reproduction

1. Create a workbench with an image that does not exist (or one that fails to pull).
2. The pod enters `ImagePullBackOff` — it is never ready.
3. Edit the workbench in the dashboard and select a different, valid image.
4. Click Save.
5. The Notebook CR spec and StatefulSet spec are both updated with the new image.
6. The pod is **not** replaced — it stays stuck pulling the old image.

## Root cause

The kf-notebook-controller has explicit support for forced pod restarts via the
`notebooks.opendatahub.io/notebook-restart` annotation (see
`components/notebook-controller/controllers/notebook_controller.go:259-286`).
When this annotation is set to `"true"`, the controller deletes the pod and
clears the annotation.

The dashboard does not set this annotation when the image is changed. The
StatefulSet's `RollingUpdate` strategy requires the current pod to be ready
before it will terminate it and create a replacement — so if the pod was never
ready (e.g. `ImagePullBackOff`), the rollout deadlocks.

## Expected behavior

When the dashboard saves an image change on a workbench, it should also set
`notebooks.opendatahub.io/notebook-restart: "true"` on the Notebook CR so the
controller deletes the stuck pod and a new one is created with the updated image.

## Affected components

- **odh-dashboard** — does not set the restart annotation on image change
- **kf-notebook-controller** — working as designed (restart mechanism exists but is not triggered)

## Workaround

Manually delete the pod:

```sh
kubectl delete pod <workbench>-0 -n <namespace>
```

Or set the annotation:

```sh
kubectl annotate notebook <workbench> -n <namespace> \
  notebooks.opendatahub.io/notebook-restart=true
```

## Environment

Discovered on picoshift (kind + ocp-sim) running ODH operator v2.25.0.
Likely reproducible on OpenShift when a workbench image fails to pull.
