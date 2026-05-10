from dataclasses import dataclass, field


@dataclass
class CellResult:
    index: int
    cell_type: str
    source: str
    outputs: list[str] = field(default_factory=list)
    error: str | None = None
    execution_time: float = 0.0

    @property
    def ok(self) -> bool:
        return self.error is None

    def summary(self) -> str:
        status = "OK" if self.ok else f"FAIL: {self.error}"
        src_preview = self.source[:60].replace("\n", "\\n")
        return f"[{self.index}] {self.cell_type} {status} ({self.execution_time:.1f}s) {src_preview}"
