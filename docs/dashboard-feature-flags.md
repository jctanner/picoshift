# ODH Dashboard Feature Flags

## `devFeatureFlags` URL Parameter

The dashboard supports a `devFeatureFlags` query parameter to toggle feature flags
at runtime. Flags are persisted in session storage after the first load.

### Format

```
?devFeatureFlags=flagName=true,anotherFlag=false
```

Special values:
- `?devFeatureFlags=true` — enable all flags
- `?devFeatureFlags=false` — disable all flags

### MaaS-Related Flags

| Flag | Default | Controls |
|------|---------|----------|
| `modelAsService` | `true` | Core MaaS UI — "Gen AI studio" nav section, "API keys" page |
| `maasAuthPolicies` | `true` | "Authorization policies" under Settings (admin only) |
| `vLLMDeploymentOnMaaS` | `false` | vLLM deployment option in model serving wizard |

### Navigation Items

When MaaS is enabled, these nav items appear:

```
Gen AI studio
  └── API keys              /maas/tokens        (all users)

Settings
  ├── Subscriptions         /maas/subscriptions  (admin only)
  └── Authorization policies /maas/auth-policies  (admin + maasAuthPolicies flag)
```

### Implementation

- Flag definitions: `frontend/src/concepts/areas/const.ts` (techPreviewFlags)
- Area enum: `frontend/src/concepts/areas/types.ts` (SupportedArea)
- URL param parsing: `frontend/src/app/featureFlags/useDevFeatureFlags.ts`
- Nav extensions: `packages/maas/frontend/src/odh/odhExtensions/odhExtensions.ts`
- Backend defaults: `backend/src/utils/constants.ts` (blankDashboardCR)

### Area Activation

MaaS areas also check the DSC status condition `ModelsAsServiceReady`. Even with
the feature flag enabled, the nav items won't appear unless the DSC reports MaaS
as ready.

## Known Issues

### maas-ui BFF cannot reach maas-api

The `maas-ui` sidecar proxies API calls to `https://maas.apps.ocp-sim.test`.
Inside the pod, `*.localhost` resolves to `[::1]` (IPv6 loopback), so the
connection is refused. The BFF needs to reach the maas-api through the MaaS
gateway, but DNS doesn't route that hostname correctly from inside the cluster.

Error:
```
Post "https://maas.apps.ocp-sim.test/maas-api/v1/api-keys/search":
  dial tcp [::1]:443: connect: connection refused
```

Status: investigating how the BFF determines its upstream URL.
