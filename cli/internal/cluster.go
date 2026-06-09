package internal

import (
	"fmt"
	"strings"
	"time"
)

func ClusterState(clusterName string) (containerUp, simDeployed bool) {
	containerUp = containerRunning(clusterName)
	if containerUp {
		simDeployed = hasPicoshiftSim(clusterName)
	}
	return
}

func IsRunning(root, clusterName string) bool {
	c, s := ClusterState(clusterName)
	return c && s
}

func containerRunning(clusterName string) bool {
	containerName := clusterName + "-control-plane"
	out, err := RunOutputQuiet("podman", "inspect", "-f", "{{.State.Running}}", containerName)
	if err == nil && strings.TrimSpace(out) == "true" {
		return true
	}
	sudo := Sudo()
	if sudo != "" {
		out, err = RunOutputQuiet(sudo, "podman", "inspect", "-f", "{{.State.Running}}", containerName)
		if err == nil && strings.TrimSpace(out) == "true" {
			return true
		}
	}
	return false
}

func KindKnows(root, clusterName string) bool {
	kindBin := ResolvedKindBin(root)
	sudo := Sudo()
	var out string
	var err error
	if sudo == "" {
		out, err = RunOutputQuiet(kindBin, "get", "clusters")
	} else {
		out, err = RunOutputQuiet(sudo, kindBin, "get", "clusters")
	}
	if err != nil {
		return false
	}
	for _, line := range strings.Split(out, "\n") {
		if strings.TrimSpace(line) == clusterName {
			return true
		}
	}
	return false
}

func hasPicoshiftSim(clusterName string) bool {
	ctx := "kind-" + clusterName
	return RunQuiet("kubectl", "--context", ctx, "-n", SimNamespace,
		"get", "daemonset", "ocp-sim") == nil
}

func GetAuthMode(context string) string {
	out, err := RunOutputQuiet("kubectl", "--context", context, "-n", SimNamespace,
		"get", "daemonset", "ocp-sim",
		"-o", "jsonpath={.spec.template.spec.containers[0].args}")
	if err != nil {
		return "unknown"
	}
	if !strings.Contains(out, "--auth-mode") {
		return "legacy"
	}
	// Args come back as a JSON array: ["--proxy","--auth-mode","byoidc",...]
	// Split on comma to handle both JSON and space-separated formats.
	parts := strings.FieldsFunc(out, func(r rune) bool {
		return r == ',' || r == ' '
	})
	for i, p := range parts {
		clean := strings.Trim(p, "\"'[]")
		if clean == "--auth-mode" && i+1 < len(parts) {
			return strings.Trim(parts[i+1], "\"'[]")
		}
	}
	return "legacy"
}

func WaitForNode(timeout time.Duration) error {
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		if err := RunQuiet("kubectl", "get", "nodes", "--request-timeout=5s"); err == nil {
			break
		}
		time.Sleep(5 * time.Second)
	}
	remaining := time.Until(deadline)
	if remaining < 10*time.Second {
		remaining = 10 * time.Second
	}
	return Run("kubectl", "wait", "--for=condition=Ready", "node", "--all",
		fmt.Sprintf("--timeout=%ds", int(remaining.Seconds())))
}

func ExportKubeconfig(root, clusterName string) error {
	kindBin := ResolvedKindBin(root)
	home, err := RunOutput("sh", "-c", "echo ~$(id -un)")
	if err != nil {
		return err
	}
	kubeconfigPath := home + "/.kube/config"
	out, err := RunSudoOutput(kindBin, "get", "kubeconfig", "--name", clusterName)
	if err != nil {
		return fmt.Errorf("failed to get kubeconfig: %w", err)
	}
	return writeFile(kubeconfigPath, out)
}

func writeFile(path, content string) error {
	return runBash(fmt.Sprintf("cat > %s << 'KUBECONFIG_EOF'\n%s\nKUBECONFIG_EOF", path, content))
}

func runBash(script string) error {
	return Run("bash", "-c", script)
}

func NamespaceExists(ns, context string) bool {
	return RunQuiet("kubectl", "--context", context, "get", "namespace", ns) == nil
}
