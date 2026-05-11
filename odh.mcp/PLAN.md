# ODH MCP Server — `./odh.mcp/`

## Context

We need an in-cluster MCP server that orchestrates the RHOAI fraud detection tutorial end-to-end. It lives in the project namespace, speaks to workbenches via the Jupyter API (WebSocket + REST), and to Kubernetes for resource management. Agents connect to it for interactive exploration; scripts use the same tools for CI automation. This validates the full picoshift stack: proxy, WebSocket, kernel execution, S3 connectivity, model serving.

Starting with workbench orchestration (Phase 1), it grows to cover model deployment and testing.

## Architecture

```
Agent / Script
    │
    ▼ (MCP over stdio or HTTP)
┌─────────────────────────────────┐
│  odh.mcp server                 │
│  ┌───────────┐ ┌──────────────┐ │
│  │ Notebook   │ │ Kubernetes   │ │
│  │ Tools      │ │ Tools        │ │
│  └─────┬─────┘ └──────┬───────┘ │
│        │               │        │
│  jupyter-server-client  │        │
│  jupyter-kernel-client  kubectl  │
└────────┼───────────────┼────────┘
         │               │
         ▼               ▼
   Workbench Pod    K8s API Server
   (Jupyter)
```

Runs on the host (not in-cluster for Phase 1) — it needs access to both the Jupyter workbench URL and kubeconfig.

## Phase 1 — Files to create

### `odh.mcp/pyproject.toml`

```toml
[project]
name = "odh-mcp"
version = "0.1.0"
requires-python = ">=3.10"
dependencies = [
    "mcp[cli]>=1.10.1",
    "jupyter-kernel-client>=0.9.0",
    "jupyter-server-client>=0.1.1",
    "pydantic>=2.0",
]

[project.scripts]
odh-mcp = "odh_mcp.server:main"
```

### `odh.mcp/odh_mcp/__init__.py`

Empty.

### `odh.mcp/odh_mcp/server.py`

The MCP server with tool registration. Uses `FastMCP` from the `mcp` SDK.

```python
from mcp.server import FastMCP

mcp = FastMCP(name="ODH MCP Server")

# Register notebook tools
# Register kubernetes tools (Phase 2)

def main():
    mcp.run(transport="stdio")
```

Tools registered via `@mcp.tool()` decorator, same pattern as jupyter-mcp-server.

### `odh.mcp/odh_mcp/config.py`

Pydantic config model:

```python
class ODHMCPConfig(BaseModel):
    workbench_url: str          # e.g. "http://localhost:8888" or full proxy URL
    workbench_token: str = ""   # Jupyter token (empty if auth disabled)
    namespace: str = ""         # K8s namespace for resource tools
    kubeconfig: str | None = None
```

Set via CLI args or env vars (`ODH_MCP_WORKBENCH_URL`, etc).

### `odh.mcp/odh_mcp/notebook.py`

Notebook management — thin wrapper around `jupyter-server-client` and `jupyter-kernel-client`:

```python
class NotebookRunner:
    """Connects to a Jupyter server, manages kernels, executes cells."""
    
    def __init__(self, server_url: str, token: str = ""):
        self.client = JupyterServerClient(base_url=server_url, token=token)
        self.kernel = None
    
    async def open_notebook(self, path: str) -> dict:
        """Read notebook contents via REST API."""
    
    async def start_kernel(self) -> str:
        """Start a kernel, return kernel_id."""
    
    async def execute_cell(self, cell_index: int, timeout: int = 60) -> CellResult:
        """Execute a single cell, return outputs + error info."""
    
    async def execute_all(self, path: str) -> list[CellResult]:
        """Execute all code cells sequentially, stop on error."""
    
    async def shutdown(self):
        """Kill kernel, clean up."""
```

Borrows execution logic from `example.src/jupyter-mcp-server/jupyter_mcp_server/tools/execute_cell_tool.py` (MCP_SERVER mode branch, lines 249-403) and `use_notebook_tool.py` (lines 192-205 for KernelClient setup).

### `odh.mcp/odh_mcp/tools/` — Tool implementations

#### `notebook_tools.py` — Workbench/notebook tools

| Tool | Description |
|------|-------------|
| `list_notebooks` | List .ipynb files on the workbench |
| `read_notebook` | Read all cells (source + outputs) from a notebook |
| `read_cell` | Read a single cell's source and outputs |
| `execute_cell` | Execute one cell by index, return outputs |
| `execute_notebook` | Execute all code cells sequentially, return per-cell results |
| `execute_code` | Execute arbitrary code in the kernel |

#### `kube_tools.py` — Kubernetes tools (Phase 2, stubs for now)

| Tool | Description |
|------|-------------|
| `get_pod_status` | Check pod readiness in namespace |
| `get_inferenceservice` | Get ISVC status/conditions |
| `deploy_model` | Create ServingRuntime + InferenceService |
| `test_inference` | Send prediction request to model endpoint |
| `create_connection` | Create S3 connection secret |

### `odh.mcp/odh_mcp/models.py`

```python
@dataclass
class CellResult:
    index: int
    cell_type: str        # "code" or "markdown"
    source: str
    outputs: list[str]
    error: str | None
    execution_time: float
```

## How it connects to the workbench

The workbench runs at `https://rh-ai.apps.ocp-sim.test/notebook/fraud-detection/fraud-detection/`. The Jupyter REST API is at that base URL + `/api/...`.

The `jupyter-server-client` library handles:
- `GET /api/contents/{path}` — read notebook JSON
- `POST /api/kernels` — start kernel
- `GET /api/kernels` — list kernels

The `jupyter-kernel-client` library handles:
- WebSocket connection to `/api/kernels/{id}/channels`
- Sending `execute_request` messages
- Receiving `stream`, `execute_result`, `error`, `status` messages

This exercises the full proxy → WebSocket → kernel stack.

## Usage

**As MCP server for Claude Code / agents:**
```bash
cd odh.mcp && pip install -e .
# Add to Claude Code MCP config:
# odh-mcp --workbench-url https://rh-ai.apps.ocp-sim.test/notebook/fraud-detection/fraud-detection
```

**As library from scripts:**
```python
from odh_mcp.notebook import NotebookRunner

runner = NotebookRunner("https://rh-ai.apps.ocp-sim.test/notebook/fraud-detection/fraud-detection")
await runner.start_kernel()
results = await runner.execute_all("fraud-detection/1_experiment_train.ipynb")
for r in results:
    print(f"Cell {r.index}: {'OK' if not r.error else 'FAIL: ' + r.error}")
```

**From deploy-fraud-tutorial.py:**
```python
# After workbench is ready, use NotebookRunner to:
# 1. Clone the fraud-detection repo (execute_code)
# 2. Run training notebook (execute_all)
# 3. Run save-model notebook (execute_all)
# 4. Verify model in S3 (execute_code with boto3)
```

## Verification

```bash
# 1. Install the package
cd odh.mcp && pip install -e .

# 2. Ensure a workbench is running
make workbench WORKBENCH_PROJECT=fraud-detection WORKBENCH_IMAGE=jupyter-tensorflow-notebook:3.4

# 3. Test the MCP server
odh-mcp --workbench-url https://rh-ai.apps.ocp-sim.test/notebook/fraud-detection/fraud-detection

# 4. From another terminal, test with MCP client or use as library:
python -c "
import asyncio
from odh_mcp.notebook import NotebookRunner
async def test():
    r = NotebookRunner('https://rh-ai.apps.ocp-sim.test/notebook/fraud-detection/fraud-detection')
    await r.start_kernel()
    result = await r.execute_code('print(\"hello from picoshift\")')
    print(result)
    await r.shutdown()
asyncio.run(test())
"
```

## Reference code to borrow from

| What | Source file |
|------|------------|
| KernelClient setup | `example.src/jupyter-mcp-server/jupyter_mcp_server/tools/use_notebook_tool.py:192-205` |
| Cell execution (MCP_SERVER mode) | `example.src/jupyter-mcp-server/jupyter_mcp_server/tools/execute_cell_tool.py:249-403` |
| Notebook reading | `example.src/jupyter-mcp-server/jupyter_mcp_server/tools/read_notebook_tool.py` |
| Cell reading | `example.src/jupyter-mcp-server/jupyter_mcp_server/tools/read_cell_tool.py` |
| Output extraction | `example.src/jupyter-mcp-server/jupyter_mcp_server/utils.py` (safe_extract_outputs, extract_output) |
| FastMCP tool registration | `example.src/jupyter-mcp-server/jupyter_mcp_server/server.py:233-263` |
| Config pattern | `example.src/jupyter-mcp-server/jupyter_mcp_server/config.py` |
