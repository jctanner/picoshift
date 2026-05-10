"""CLI entry point for the ODH MCP server."""

import argparse
import sys

from .config import ODHMCPConfig, set_config
from .server import main as run_server


def main():
    parser = argparse.ArgumentParser(description="ODH MCP Server")
    parser.add_argument("--workbench-url", help="Jupyter workbench URL")
    parser.add_argument("--workbench-token", default="", help="Jupyter auth token")
    parser.add_argument("--namespace", default="", help="Kubernetes namespace")
    parser.add_argument("--kubeconfig", default=None, help="Path to kubeconfig")
    parser.add_argument("--no-verify-ssl", action="store_true", help="Disable SSL certificate verification")
    args = parser.parse_args()

    config = ODHMCPConfig.from_env()
    if args.workbench_url:
        config.workbench_url = args.workbench_url
    if args.workbench_token:
        config.workbench_token = args.workbench_token
    if args.namespace:
        config.namespace = args.namespace
    if args.kubeconfig:
        config.kubeconfig = args.kubeconfig
    if args.no_verify_ssl:
        config.verify_ssl = False

    if not config.workbench_url:
        print("Error: workbench URL required (--workbench-url or ODH_MCP_WORKBENCH_URL)", file=sys.stderr)
        sys.exit(1)

    set_config(config)
    run_server()


if __name__ == "__main__":
    main()
