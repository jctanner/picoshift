package cmd

import (
	"fmt"
	"path/filepath"

	"github.com/jctanner/picoshift/internal"
	"github.com/spf13/cobra"
)

func NewDeleteCmd() *cobra.Command {
	var name string

	cmd := &cobra.Command{
		Use:   "delete",
		Short: "Delete the cluster or namespace-mode resources",
		RunE: func(cmd *cobra.Command, args []string) error {
			if err := internal.ValidateName(name); err != nil {
				return err
			}
			statePath := internal.StatePath(name)
			state, err := internal.LoadState(statePath)
			if err != nil {
				return fmt.Errorf("failed to load state: %w", err)
			}

			if state.Mode == internal.ModeNamespace {
				return deleteNamespaceMode(name)
			}

			return deleteKindMode(name)
		},
	}

	cmd.Flags().StringVar(&name, "name", internal.DefaultClusterName, "Cluster name")

	return cmd
}

func deleteKindMode(name string) error {
	root, err := internal.ProjectRoot()
	if err != nil {
		return err
	}
	if !internal.KindKnows(root, name) {
		fmt.Printf("Cluster %q not found, nothing to do.\n", name)
		return nil
	}
	fmt.Printf("=== Deleting kind cluster %q ===\n", name)
	kindBin := internal.ResolvedKindBin(root)
	if err := internal.RunSudo(kindBin, "delete", "cluster", "--name", name); err != nil {
		return err
	}
	_ = internal.RemoveState(internal.StatePath(name))
	return nil
}

func deleteNamespaceMode(name string) error {
	root, err := internal.ProjectRoot()
	if err != nil {
		return err
	}

	if err := internal.RunQuiet("kubectl", "cluster-info"); err != nil {
		return fmt.Errorf("cannot reach cluster. Fix kubeconfig before deleting namespace-mode resources: %w", err)
	}

	fmt.Printf("=== Deleting namespace-mode deployment %q ===\n", name)

	fmt.Println("  Removing admin RBAC...")
	_ = internal.RunQuiet("kubectl", "delete", "clusterrolebinding",
		"ocp-sim-admin", "--ignore-not-found")

	fmt.Println("  Removing simulator...")
	_ = internal.RunQuiet("kubectl", "delete", "-f",
		filepath.Join(root, "deploy/simulator-namespace.yaml"),
		"--ignore-not-found", "--timeout=30s")

	fmt.Println("  Removing seed resources...")
	seedDir := filepath.Join(root, "deploy/seed")
	for _, f := range []string{
		"rbac-compat.yaml", "jobset-operator.yaml", "sccs.yaml",
		"infrastructure.yaml", "ingress.yaml", "htpasswd.yaml",
		"authentication-oidc.yaml", "authentication.yaml",
		"cluster-config.yaml",
	} {
		_ = internal.RunQuiet("kubectl", "delete", "-f",
			filepath.Join(seedDir, f), "--ignore-not-found")
	}

	fmt.Println("  Removing ClusterVersion...")
	_ = internal.RunQuiet("kubectl", "delete", "clusterversion", "version", "--ignore-not-found")

	fmt.Println("  Removing CRDs...")
	crdsDir := filepath.Join(root, "deploy/crds")
	for _, d := range internal.CRDDirs {
		if d == "olm" && isOLMInstalled() {
			fmt.Println("  OLM is running, skipping OLM CRD removal")
			continue
		}
		_ = internal.RunQuiet("kubectl", "delete", "-f",
			filepath.Join(crdsDir, d), "--ignore-not-found", "--timeout=30s")
	}
	_ = internal.RunQuiet("kubectl", "delete", "-f",
		filepath.Join(crdsDir, "jobset"), "--ignore-not-found", "--timeout=30s")

	fmt.Println("  Removing picoshift namespace...")
	_ = internal.RunQuiet("kubectl", "delete", "namespace",
		internal.SimNamespace, "--ignore-not-found", "--timeout=60s")

	_ = internal.RemoveState(internal.StatePath(name))
	fmt.Println("=== Deleted ===")
	return nil
}
