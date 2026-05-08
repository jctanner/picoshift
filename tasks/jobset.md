# JobSet Simulation Roadmap

## Background

The JobSet API (`jobset.x-k8s.io/v1alpha2`) manages groups of related batch Jobs
for distributed training workloads (e.g., PyTorch multi-node training). The ODH
trainer component requires the JobSet operator to be installed — without it, the
DSC stays NotReady with `provisioning failed: JobSet operator not installed`.

## Current State: Partial Mock (Phase 1)

The simulator includes a partial mock controller in `simulator/src/jobset.rs`
that handles the core lifecycle:

**What works:**
- Watches `JobSet` resources across all namespaces
- Parses `spec.replicatedJobs` and creates real `batch/v1 Job` child resources
- Names child Jobs as `{jobset}-{replicatedJobName}-{index}`
- Sets owner references for cascading deletion
- Copies job/pod templates from the replicatedJob spec
- Tracks child Job status (succeeded/failed counts)
- Sets `terminalState: Completed` or `Failed` when all Jobs finish
- Sets Running/Completed/Failed conditions with timestamps
- Skips suspended JobSets and those already in terminal state
- Requeues every 5s while Jobs are running

**What's missing:**
- No headless Service for inter-pod DNS discovery
- No `startupPolicy` support (all Jobs created immediately)
- No `successPolicy` / `failurePolicy` evaluation
- No `replicatedJobsStatus` field in status
- No `suspend` → `unsuspend` transition handling (resume)
- No `managedBy` field support
- No exclusive placement / topology awareness
- No `minAvailable` or dependency ordering between replicatedJobs

## Phase 2: Status Enrichment

Add richer status reporting without changing the core lifecycle:

- Populate `status.replicatedJobsStatus` with per-replicatedJob counts
  (ready, succeeded, failed, active, suspended)
- Add `restarts` and `startTime` to status
- Track `observedGeneration` for spec changes

## Phase 3: Headless Service

Create a headless Service per JobSet for DNS-based pod discovery:

- Service name: `{jobset-name}`
- Selector: `jobset.x-k8s.io/jobset-name: {name}`
- ClusterIP: None
- Ports from the replicatedJob's container ports

This enables pods to discover each other via DNS, which is important for
distributed training frameworks that use hostnames for coordination.

## Phase 4: Startup Policy

Implement `spec.startupPolicy` for coordinated Job launch:

- `InOrder`: Start replicatedJobs sequentially — wait for previous group to
  have all pods Ready before creating the next group
- `AnyOrder` (default): Current behavior, all Jobs created immediately

This matters for training setups where a parameter server must be running
before workers start.

## Phase 5: Success/Failure Policies

Implement `spec.successPolicy` and `spec.failurePolicy`:

- **successPolicy**: Which replicatedJobs must succeed for the JobSet to be
  considered successful (e.g., only the `driver` job, not all workers)
- **failurePolicy**: How many failures to tolerate before marking the JobSet
  as failed, with optional `maxRestarts` for automatic retry

## Phase 6: Suspend/Resume

Handle `spec.suspend` changes:

- When `suspend` goes from false → true: suspend all child Jobs
- When `suspend` goes from true → false: resume child Jobs, optionally
  re-evaluating startup policy

## Full Simulation (Stretch)

For truly realistic training simulation:

- Exclusive placement annotations for GPU topology
- Network policy integration for training traffic
- Simulated training duration (configurable delay before Job completion)
- Resource quota awareness
- Integration with Kueue for workload scheduling

## Notes

The partial mock (Phase 1) is sufficient for the ODH operator to consider the
trainer component healthy. The DSC will show `TrainerReady: True` once the
JobSet CRD exists and the API is functional. Subsequent phases improve realism
for dashboard UI testing and operator integration testing.
