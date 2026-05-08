#!/usr/bin/env python3
"""Patch GatewayConfig to allow self-signed IDP certificates.

On picoshift the OAuth server uses a self-signed CA, so kube-auth-proxy
must be told not to verify the provider certificate.
"""

import subprocess
import sys


def main():
    print("Patching GatewayConfig to accept self-signed IDP certs...")

    result = subprocess.run(
        [
            "kubectl", "patch", "gatewayconfig", "default-gateway",
            "--type=merge",
            "-p", '{"spec":{"verifyProviderCertificate":false}}',
        ],
        capture_output=True, text=True,
    )

    if result.returncode != 0:
        print(f"  ERROR: {result.stderr.strip()}", file=sys.stderr)
        sys.exit(1)

    print(f"  {result.stdout.strip()}")
    print("Done.")


if __name__ == "__main__":
    main()
