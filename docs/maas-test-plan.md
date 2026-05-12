# MaaS Authn/Authz Test Plan Without a Real Model Server

## Goal

Test MaaS gateway authentication and authorization without deploying a real
model server. The backend only needs to prove whether a request reached the
model route and what headers were forwarded after gateway policy evaluation.

## Approach

Deploy a small OpenAI-compatible mock backend and expose it through the MaaS
gateway as a model. The mock returns static responses for inference endpoints
and records or echoes request headers for inspection.

This lets us validate:

- unauthenticated requests are denied before the backend
- invalid keys are denied before the backend
- valid API keys reach the backend
- model/subscription authorization is enforced
- client-supplied identity headers cannot spoof gateway identity
- server-resolved subscription identity wins over client-supplied headers

## Mock Backend

The mock backend should expose:

```text
GET  /health
GET  /ready
GET  /v1/models
POST /v1/chat/completions
POST /v1/completions
```

Successful inference can return static OpenAI-style JSON:

```json
{
  "id": "mock-completion",
  "object": "chat.completion",
  "model": "mock/model",
  "choices": [
    {
      "index": 0,
      "message": {
        "role": "assistant",
        "content": "mock response"
      },
      "finish_reason": "stop"
    }
  ]
}
```

The backend should also expose enough observability to determine whether it was
reached, such as:

- logging request method, path, and selected headers
- echoing selected headers in a debug response
- incrementing a request counter

Do not log full bearer tokens or API keys.

## Resource Shape

Use the `ExternalModel` path so the test does not depend on a real
`LLMInferenceService`, KServe runtime, GPU, or model container. The
`ExternalModel` reconciler can route to an in-cluster mock service by using the
service DNS name as the endpoint and disabling TLS origination.

Example:

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: mock-provider-key
  namespace: llm
type: Opaque
stringData:
  api-key: mock-provider-key
---
apiVersion: maas.opendatahub.io/v1alpha1
kind: ExternalModel
metadata:
  name: mock-openai
  namespace: llm
  annotations:
    maas.opendatahub.io/port: "8080"
    maas.opendatahub.io/tls: "false"
spec:
  provider: openai
  targetModel: mock/model
  endpoint: maas-mock.llm.svc.cluster.local
  credentialRef:
    name: mock-provider-key
---
apiVersion: maas.opendatahub.io/v1alpha1
kind: MaaSModelRef
metadata:
  name: mock-openai
  namespace: llm
spec:
  modelRef:
    kind: ExternalModel
    name: mock-openai
```

Grant access with a normal MaaS auth policy and subscription:

```yaml
apiVersion: maas.opendatahub.io/v1alpha1
kind: MaaSAuthPolicy
metadata:
  name: mock-access
  namespace: models-as-a-service
spec:
  modelRefs:
    - name: mock-openai
      namespace: llm
  subjects:
    groups:
      - name: system:authenticated
    users: []
---
apiVersion: maas.opendatahub.io/v1alpha1
kind: MaaSSubscription
metadata:
  name: mock-subscription
  namespace: models-as-a-service
spec:
  owner:
    groups:
      - name: system:authenticated
    users: []
  modelRefs:
    - name: mock-openai
      namespace: llm
      tokenRateLimits:
        - limit: 100
          window: 1m
  priority: 10
```

Expected gateway path:

```text
https://maas.apps.ocp-sim.test/llm/mock-openai/v1/chat/completions
```

## Test Matrix

| Case | Request | Expected result |
| --- | --- | --- |
| No auth | No `Authorization` header | `401` or `403`; mock backend is not reached |
| Invalid API key | `Authorization: Bearer invalid` | `401` or `403`; mock backend is not reached |
| Valid key | API key bound to `mock-subscription` | `200`; mock backend is reached |
| Wrong subscription/model | Valid key for a subscription that does not include `mock-openai` | `401` or `403`; mock backend is not reached |
| Header spoofing | Valid key plus client-supplied `X-MaaS-Username`, `X-MaaS-Group`, `X-MaaS-Key-Id` | Request succeeds using real key identity; mock does not see attacker-controlled trusted identity |
| Subscription spoofing | Valid key plus client-supplied `X-MaaS-Subscription` | Server-resolved subscription wins |
| Policy removal | Delete `MaaSAuthPolicy/mock-access` | Subsequent request is denied |

## Assertions

For denied requests:

- response is `401` or `403`
- mock backend logs/counter show no request

For allowed requests:

- response is `200`
- mock backend logs/counter show exactly one request
- path is the expected inference path
- client-supplied sensitive identity headers are stripped or replaced according
  to MaaS policy
- request does not expose the original MaaS API key to the backend unless that
  is explicitly intended by the current policy design

## Follow-Up Checks

After the mock path works, use it to verify gateway-generated resources:

```bash
kubectl get externalmodel,maasmodelref,maasauthpolicy,maassubscription -A
kubectl get httproute -n llm mock-openai -o yaml
kubectl get authpolicy,tokenratelimitpolicy -A
```

Also confirm the model appears in the MaaS API catalog:

```bash
curl -sk https://maas.apps.ocp-sim.test/maas-api/v1/models \
  -H "Authorization: Bearer ${API_KEY}" | jq .
```
