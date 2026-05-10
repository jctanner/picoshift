"""Notebook management — connects to a Jupyter server, manages kernels, executes cells."""

import asyncio
import json
import logging
import os
import re
import ssl
import time

import requests as _requests
import websocket as _ws_module
from jupyter_kernel_client import KernelClient
from jupyter_server_client import JupyterServerClient

from .models import CellResult

logger = logging.getLogger(__name__)


def _strip_ansi(text: str) -> str:
    return re.sub(r"\x1b\[[0-9;]*m", "", text)


def _extract_output(output: dict) -> str:
    """Extract readable text from a single Jupyter cell output dict."""
    output_type = output.get("output_type", "")

    if output_type == "stream":
        text = output.get("text", "")
        if isinstance(text, list):
            text = "".join(text)
        return _strip_ansi(str(text))

    if output_type in ("display_data", "execute_result"):
        data = output.get("data", {})
        if "text/plain" in data:
            text = data["text/plain"]
            if isinstance(text, list):
                text = "".join(text)
            return _strip_ansi(str(text))
        if "text/html" in data:
            return "[HTML output]"
        if "image/png" in data:
            return "[Image output (PNG)]"
        return str(data)

    if output_type == "error":
        tb = output.get("traceback", [])
        if isinstance(tb, list):
            return _strip_ansi("\n".join(tb))
        return _strip_ansi(str(tb))

    return str(output)


def _extract_outputs(outputs: list) -> list[str]:
    """Extract all outputs from a cell."""
    result = []
    for out in outputs:
        text = _extract_output(out)
        if text.strip():
            result.append(text)
    return result


class NotebookRunner:
    """Connects to a Jupyter server, manages kernels, and executes notebook cells.

    Can be used as a library from scripts or as the backend for MCP tools.
    """

    def __init__(self, server_url: str, token: str = "", verify_ssl: bool = True):
        self.server_url = server_url.rstrip("/")
        self.token = token
        self.verify_ssl = verify_ssl
        self.client = JupyterServerClient(
            base_url=self.server_url, token=self.token, verify_ssl=self.verify_ssl,
        )
        if not verify_ssl:
            _ws_module.enableTrace(False)
            self._orig_sslopt = {"cert_reqs": ssl.CERT_NONE, "check_hostname": False}
        else:
            self._orig_sslopt = {}
        self._xsrf_token: str | None = None
        self._kernel: KernelClient | None = None
        self._notebook_cells: list[dict] | None = None
        self._notebook_path: str | None = None

    def _fetch_xsrf_token(self) -> str:
        """Fetch XSRF token from the Jupyter server (required for POST/PUT/DELETE)."""
        if self._xsrf_token:
            return self._xsrf_token
        headers = {}
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"
        resp = _requests.get(
            f"{self.server_url}/tree",
            headers=headers,
            verify=self.verify_ssl,
            allow_redirects=True,
        )
        xsrf = resp.cookies.get("_xsrf", "")
        if not xsrf:
            logger.warning("No _xsrf cookie received from Jupyter server")
        self._xsrf_token = xsrf
        return xsrf

    @property
    def kernel_id(self) -> str | None:
        if self._kernel is None:
            return None
        return getattr(self._kernel, "id", None)

    def list_files(self, path: str = "") -> list[dict]:
        """List files on the Jupyter server."""
        items = self.client.contents.list_directory(path)
        return [
            {"name": item.name, "type": item.type, "path": f"{path}/{item.name}" if path else item.name}
            for item in items
        ]

    def list_notebooks(self, path: str = "", recursive: bool = True) -> list[str]:
        """List .ipynb files on the Jupyter server."""
        notebooks = []
        items = self.client.contents.list_directory(path)
        for item in items:
            full_path = f"{path}/{item.name}" if path else item.name
            if item.type == "notebook":
                notebooks.append(full_path)
            elif item.type == "directory" and recursive:
                notebooks.extend(self.list_notebooks(full_path, recursive=True))
        return notebooks

    def read_notebook(self, path: str) -> dict:
        """Read a notebook file and return its parsed JSON content."""
        content = self.client.contents.get(path)
        if hasattr(content, "content"):
            nb = content.content
        else:
            nb = content
        if isinstance(nb, str):
            nb = json.loads(nb)
        self._notebook_path = path
        self._notebook_cells = nb.get("cells", [])
        return nb

    def get_cells(self) -> list[dict]:
        """Return cells from the last read_notebook call."""
        if self._notebook_cells is None:
            raise RuntimeError("No notebook loaded — call read_notebook() first")
        return self._notebook_cells

    def start_kernel(self, path: str | None = None, retries: int = 5) -> str:
        """Start a new kernel and return its ID."""
        xsrf = self._fetch_xsrf_token()
        headers = {}
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"
        if xsrf:
            headers["X-XSRFToken"] = xsrf
            headers["Cookie"] = f"_xsrf={xsrf}"
        if not self.verify_ssl:
            self._patch_websocket_ssl()

        last_err = None
        for attempt in range(retries):
            try:
                self._kernel = KernelClient(
                    server_url=self.server_url,
                    token=self.token,
                    headers=headers,
                )
                self._kernel.start(path=path or self._notebook_path or "")
                kid = getattr(self._kernel, "id", "unknown")
                logger.info(f"Kernel started: {kid}")
                return kid
            except Exception as e:
                last_err = e
                self._kernel = None
                if attempt < retries - 1:
                    wait = 5 * (attempt + 1)
                    logger.warning(f"Kernel start failed (attempt {attempt+1}): {e}, retrying in {wait}s")
                    time.sleep(wait)
                    self._xsrf_token = None
                    xsrf = self._fetch_xsrf_token()
                    if xsrf:
                        headers["X-XSRFToken"] = xsrf
                        headers["Cookie"] = f"_xsrf={xsrf}"
        raise last_err

    def _patch_websocket_ssl(self):
        """Patch websocket-client and jupyter_kernel_client to skip SSL verification."""
        import jupyter_kernel_client.manager as _km

        sslopt = self._orig_sslopt

        if not getattr(_ws_module.WebSocketApp, "_ssl_patched", False):
            orig_run = _ws_module.WebSocketApp.run_forever

            def patched_run_forever(self_ws, *args, **kwargs):
                kwargs.setdefault("sslopt", {}).update(sslopt)
                return orig_run(self_ws, *args, **kwargs)

            _ws_module.WebSocketApp.run_forever = patched_run_forever
            _ws_module.WebSocketApp._ssl_patched = True

        if not getattr(_km, "_ssl_patched", False):
            orig_fetch = _km.fetch

            def patched_fetch(*args, **kwargs):
                kwargs.setdefault("verify", False)
                return orig_fetch(*args, **kwargs)

            _km.fetch = patched_fetch
            _km._ssl_patched = True

    def execute_code(self, code: str, timeout: int = 60) -> list[str]:
        """Execute arbitrary code in the kernel (synchronous)."""
        if self._kernel is None:
            raise RuntimeError("No kernel — call start_kernel() first")
        result = self._kernel.execute(code, timeout=timeout)
        if result and "outputs" in result:
            return _extract_outputs(result["outputs"])
        return []

    async def execute_code_async(self, code: str, timeout: int = 60) -> list[str]:
        """Execute code asynchronously with timeout."""
        if self._kernel is None:
            raise RuntimeError("No kernel — call start_kernel() first")

        try:
            result = await asyncio.wait_for(
                asyncio.to_thread(self._kernel.execute, code, timeout=timeout),
                timeout=timeout + 5,
            )
        except asyncio.TimeoutError:
            try:
                self._kernel.interrupt()
            except Exception:
                pass
            return [f"[TIMEOUT: execution exceeded {timeout}s]"]

        if result and "outputs" in result:
            return _extract_outputs(result["outputs"])
        return []

    async def execute_cell(self, cell_index: int, timeout: int = 60) -> CellResult:
        """Execute a single cell by index."""
        cells = self.get_cells()
        if cell_index < 0 or cell_index >= len(cells):
            raise IndexError(f"Cell index {cell_index} out of range (notebook has {len(cells)} cells)")

        cell = cells[cell_index]
        cell_type = cell.get("cell_type", "code")
        source = cell.get("source", "")
        if isinstance(source, list):
            source = "".join(source)

        if cell_type != "code" or not source.strip():
            return CellResult(
                index=cell_index,
                cell_type=cell_type,
                source=source,
            )

        start = time.monotonic()
        outputs = await self.execute_code_async(source, timeout=timeout)
        elapsed = time.monotonic() - start

        error = None
        for out in outputs:
            if out.startswith("[TIMEOUT:"):
                error = out
                break

        if self._kernel is not None:
            for out in outputs:
                if "Traceback (most recent call last)" in out:
                    if error is None:
                        error = out[:200]
                    break

        return CellResult(
            index=cell_index,
            cell_type=cell_type,
            source=source,
            outputs=outputs,
            error=error,
            execution_time=elapsed,
        )

    async def execute_notebook(
        self,
        path: str,
        timeout_per_cell: int = 120,
        stop_on_error: bool = True,
    ) -> list[CellResult]:
        """Execute all code cells in a notebook sequentially.

        Returns a list of CellResult, one per cell.
        """
        self.read_notebook(path)
        cells = self.get_cells()
        results: list[CellResult] = []

        for i, cell in enumerate(cells):
            result = await self.execute_cell(i, timeout=timeout_per_cell)
            results.append(result)
            logger.info(result.summary())

            if stop_on_error and result.error:
                logger.error(f"Stopping at cell {i}: {result.error[:100]}")
                break

        return results

    def shutdown(self) -> None:
        """Stop the kernel and clean up."""
        if self._kernel is not None:
            try:
                self._kernel.stop()
            except Exception as e:
                logger.warning(f"Error stopping kernel: {e}")
            self._kernel = None
        self._notebook_cells = None
        self._notebook_path = None

    async def __aenter__(self):
        return self

    async def __aexit__(self, *args):
        self.shutdown()
