import os
from pydantic import BaseModel


class ODHMCPConfig(BaseModel):
    workbench_url: str = ""
    workbench_token: str = ""
    namespace: str = ""
    kubeconfig: str | None = None
    verify_ssl: bool = True

    @classmethod
    def from_env(cls) -> "ODHMCPConfig":
        return cls(
            workbench_url=os.environ.get("ODH_MCP_WORKBENCH_URL", ""),
            workbench_token=os.environ.get("ODH_MCP_WORKBENCH_TOKEN", ""),
            namespace=os.environ.get("ODH_MCP_NAMESPACE", ""),
            kubeconfig=os.environ.get("KUBECONFIG"),
            verify_ssl=os.environ.get("ODH_MCP_VERIFY_SSL", "true").lower() not in ("0", "false", "no"),
        )


_config: ODHMCPConfig | None = None


def get_config() -> ODHMCPConfig:
    global _config
    if _config is None:
        _config = ODHMCPConfig.from_env()
    return _config


def set_config(config: ODHMCPConfig) -> None:
    global _config
    _config = config
