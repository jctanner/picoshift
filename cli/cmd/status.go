package cmd

import (
	"fmt"

	"github.com/jctanner/picoshift/internal"
	"github.com/spf13/cobra"
)

func NewStatusCmd() *cobra.Command {
	var name string

	cmd := &cobra.Command{
		Use:   "status",
		Short: "Show cluster state and installed components",
		RunE: func(cmd *cobra.Command, args []string) error {
			fmt.Println("=== Cluster ===")
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

			fmt.Println("\n=== Nodes ===")
			if out, err := internal.RunOutputQuiet("kubectl", "get", "nodes",
				"--context", "kind-"+name); err == nil {
				fmt.Println(out)
			} else {
				fmt.Println("  not reachable (run 'picoshift create' to set up kubeconfig)")
				return nil
			}

			ctx := "kind-" + name

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
				if internal.NamespaceExists(c.ns, ctx) {
					fmt.Printf("  %-30s installed\n", c.name)
				} else {
					fmt.Printf("  %-30s not installed\n", c.name)
				}
			}

			fmt.Println("\n=== OpenShift CRDs ===")
			if out, err := internal.RunOutputQuiet("sh", "-c",
				"kubectl --context "+ctx+" get crd -o name 2>/dev/null | grep -c 'openshift.io\\|operators.coreos.com'"); err == nil {
				fmt.Printf("  %s CRDs installed\n", out)
			}

			return nil
		},
	}

	cmd.Flags().StringVar(&name, "name", internal.DefaultClusterName, "Cluster name")

	return cmd
}
