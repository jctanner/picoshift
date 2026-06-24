package cmd

import (
	"fmt"
	"strings"

	"github.com/jctanner/picoshift/internal"
	"github.com/spf13/cobra"
)

func NewStatusCmd() *cobra.Command {
	var name string

	cmd := &cobra.Command{
		Use:   "status",
		Short: "Show cluster state and installed components",
		RunE: func(cmd *cobra.Command, args []string) error {
			statePath := internal.StatePath(name)
			state, _ := internal.LoadState(statePath)

			if state.Mode == internal.ModeNamespace {
				return statusNamespaceMode()
			}
			return statusKindMode(name)
		},
	}

	cmd.Flags().StringVar(&name, "name", internal.DefaultClusterName, "Cluster name")

	return cmd
}

func statusKindMode(name string) error {
	fmt.Println("=== Cluster (kind mode) ===")
	container, sim := internal.ClusterState(name)
	if !container {
		fmt.Printf("kind cluster %q: not found\n", name)
		return nil
	}
	if sim {
		fmt.Printf("kind cluster %q: running (picoshift)\n", name)
	} else {
		fmt.Printf("kind cluster %q: running (simulator not deployed)\n", name)
	}

	ctx := "kind-" + name

	fmt.Println("\n=== Nodes ===")
	if out, err := internal.RunOutputQuiet("kubectl", "--context", ctx,
		"get", "nodes"); err == nil {
		fmt.Println(out)
	} else {
		fmt.Println("  not reachable (run 'picoshift create' to set up kubeconfig)")
		return nil
	}

	fmt.Println("\n=== Simulator ===")
	if out, err := internal.RunOutputQuiet("kubectl", "--context", ctx,
		"-n", internal.SimNamespace, "get", "pods"); err == nil {
		fmt.Println(out)
	} else {
		fmt.Println("  not deployed")
	}

	fmt.Println("\n=== Auth Mode ===")
	mode := internal.GetAuthMode(ctx)
	fmt.Printf("  %s\n", mode)

	printComponents(ctx)
	return nil
}

func statusNamespaceMode() error {
	ns := internal.SimNamespace

	fmt.Println("=== Cluster (namespace mode) ===")
	fmt.Printf("  Namespace: %s\n", ns)

	fmt.Println("\n=== Nodes ===")
	if out, err := internal.RunOutputQuiet("kubectl", "get", "nodes"); err == nil {
		fmt.Println(out)
	} else {
		fmt.Println("  not reachable")
		return nil
	}

	fmt.Println("\n=== Simulator ===")
	if out, err := internal.RunOutputQuiet("kubectl", "-n", ns,
		"get", "pods", "-l", "app=ocp-sim"); err == nil {
		fmt.Println(out)
	} else {
		fmt.Println("  not deployed")
	}

	fmt.Println("\n=== Service ===")
	if out, err := internal.RunOutputQuiet("kubectl", "-n", ns,
		"get", "svc", "ocp-sim"); err == nil {
		fmt.Println(out)
	}

	fmt.Println("\n=== Auth Mode ===")
	mode := internal.GetAuthModeFromResource("deployment", "")
	fmt.Printf("  %s\n", mode)

	printComponents("")
	return nil
}

func printComponents(ctx string) {
	fmt.Println("\n=== Components ===")
	components := []struct {
		name string
		ns   string
	}{
		{"Gateway stack (Istio)", internal.IstioNamespace},
		{"ODH Operator", internal.OperatorNamespace},
		{"Entra Mock (BYOIDC)", internal.EntraMockNamespace},
		{"Kuadrant", "kuadrant-system"},
		{"cert-manager", "cert-manager"},
	}
	for _, c := range components {
		exists := false
		if ctx != "" {
			exists = internal.NamespaceExists(c.ns, ctx)
		} else {
			exists = internal.RunQuiet("kubectl", "get", "namespace", c.ns) == nil
		}
		if exists {
			fmt.Printf("  %-30s installed\n", c.name)
		} else {
			fmt.Printf("  %-30s not installed\n", c.name)
		}
	}

	fmt.Println("\n=== OpenShift CRDs ===")
	crdArgs := []string{"get", "crd", "-o", "name"}
	if ctx != "" {
		crdArgs = append([]string{"--context", ctx}, crdArgs...)
	}
	if out, err := internal.RunOutputQuiet("kubectl", crdArgs...); err == nil {
		count := 0
		for _, line := range strings.Split(out, "\n") {
			if strings.Contains(line, "openshift.io") || strings.Contains(line, "operators.coreos.com") {
				count++
			}
		}
		fmt.Printf("  %d CRDs installed\n", count)
	}
}
