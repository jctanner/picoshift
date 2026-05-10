"""ODH MCP Server — notebook orchestration tools for RHOAI workbenches."""

import logging
from typing import Annotated

from mcp.server import FastMCP
from pydantic import Field

from .config import get_config
from .notebook import NotebookRunner

logger = logging.getLogger(__name__)

mcp = FastMCP(name="ODH MCP Server")

_runner: NotebookRunner | None = None


def _get_runner() -> NotebookRunner:
    global _runner
    if _runner is None:
        cfg = get_config()
        if not cfg.workbench_url:
            raise RuntimeError("ODH_MCP_WORKBENCH_URL not set")
        _runner = NotebookRunner(cfg.workbench_url, cfg.workbench_token, verify_ssl=cfg.verify_ssl)
    return _runner


@mcp.tool()
async def list_notebooks(
    path: Annotated[str, Field(description="Directory path to search (empty for root)")] = "",
    recursive: Annotated[bool, Field(description="Recurse into subdirectories")] = True,
) -> str:
    """List .ipynb notebook files on the workbench."""
    runner = _get_runner()
    notebooks = runner.list_notebooks(path, recursive=recursive)
    if not notebooks:
        return "No notebooks found."
    return "\n".join(notebooks)


@mcp.tool()
async def list_files(
    path: Annotated[str, Field(description="Directory path to list (empty for root)")] = "",
) -> str:
    """List files and directories on the workbench."""
    runner = _get_runner()
    items = runner.list_files(path)
    if not items:
        return "No files found."
    lines = [f"{item['type']:>9}  {item['path']}" for item in items]
    return "\n".join(lines)


@mcp.tool()
async def read_notebook(
    path: Annotated[str, Field(description="Path to the .ipynb file on the workbench")],
) -> str:
    """Read a notebook and return a summary of all cells with their source and outputs."""
    runner = _get_runner()
    nb = runner.read_notebook(path)
    cells = nb.get("cells", [])
    parts = []
    for i, cell in enumerate(cells):
        ct = cell.get("cell_type", "unknown")
        source = cell.get("source", "")
        if isinstance(source, list):
            source = "".join(source)
        src_preview = source[:200]
        if len(source) > 200:
            src_preview += "..."
        parts.append(f"--- Cell {i} [{ct}] ---\n{src_preview}")
    return f"Notebook: {path} ({len(cells)} cells)\n\n" + "\n\n".join(parts)


@mcp.tool()
async def read_cell(
    cell_index: Annotated[int, Field(description="Zero-based cell index")],
) -> str:
    """Read a single cell's full source and any existing outputs."""
    runner = _get_runner()
    cells = runner.get_cells()
    if cell_index < 0 or cell_index >= len(cells):
        return f"Cell index {cell_index} out of range (notebook has {len(cells)} cells)"
    cell = cells[cell_index]
    ct = cell.get("cell_type", "unknown")
    source = cell.get("source", "")
    if isinstance(source, list):
        source = "".join(source)

    from .notebook import _extract_outputs

    outputs = _extract_outputs(cell.get("outputs", []))
    result = f"Cell {cell_index} [{ct}]\n\n{source}"
    if outputs:
        result += "\n\n--- Outputs ---\n" + "\n".join(outputs)
    return result


@mcp.tool()
async def start_kernel(
    path: Annotated[str, Field(description="Notebook path to associate with the kernel (optional)")] = "",
) -> str:
    """Start a new Jupyter kernel on the workbench. Required before executing code or cells."""
    runner = _get_runner()
    kid = runner.start_kernel(path=path or None)
    return f"Kernel started: {kid}"


@mcp.tool()
async def execute_cell(
    cell_index: Annotated[int, Field(description="Zero-based index of the cell to execute")],
    timeout: Annotated[int, Field(description="Execution timeout in seconds")] = 120,
) -> str:
    """Execute a single notebook cell by index and return its output."""
    runner = _get_runner()
    result = await runner.execute_cell(cell_index, timeout=timeout)
    return result.summary() + ("\n\n" + "\n".join(result.outputs) if result.outputs else "")


@mcp.tool()
async def execute_notebook(
    path: Annotated[str, Field(description="Path to the .ipynb file to execute")],
    timeout_per_cell: Annotated[int, Field(description="Timeout per cell in seconds")] = 120,
    stop_on_error: Annotated[bool, Field(description="Stop execution on first error")] = True,
) -> str:
    """Execute all code cells in a notebook sequentially. Returns per-cell results."""
    runner = _get_runner()
    results = await runner.execute_notebook(path, timeout_per_cell=timeout_per_cell, stop_on_error=stop_on_error)
    lines = [r.summary() for r in results]
    failed = [r for r in results if not r.ok]
    header = f"Executed {len(results)} cells"
    if failed:
        header += f" ({len(failed)} failed)"
    else:
        header += " (all passed)"
    return header + "\n" + "\n".join(lines)


@mcp.tool()
async def execute_code(
    code: Annotated[str, Field(description="Python code to execute in the kernel")],
    timeout: Annotated[int, Field(description="Execution timeout in seconds")] = 60,
) -> str:
    """Execute arbitrary Python code in the active kernel."""
    runner = _get_runner()
    outputs = await runner.execute_code_async(code, timeout=timeout)
    if not outputs:
        return "(no output)"
    return "\n".join(outputs)


@mcp.tool()
async def shutdown_kernel() -> str:
    """Shut down the active kernel and clean up resources."""
    runner = _get_runner()
    runner.shutdown()
    return "Kernel shut down."


def main():
    mcp.run(transport="stdio")
