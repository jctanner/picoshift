package cmd

import (
	"fmt"

	"github.com/jctanner/picoshift/internal"
	"github.com/spf13/cobra"
)

func NewDeleteCmd() *cobra.Command {
	var name string

	cmd := &cobra.Command{
		Use:   "delete",
		Short: "Delete the kind cluster",
		RunE: func(cmd *cobra.Command, args []string) error {
			root, err := internal.ProjectRoot()
			if err != nil {
				return err
			}
			if !internal.KindKnows(root, name) {
				fmt.Printf("Cluster %q not found, nothing to do.\n", name)
				return nil
			}
			fmt.Printf("=== Deleting cluster %q ===\n", name)
			kindBin := root + "/" + internal.KindBin
			return internal.RunSudo(kindBin, "delete", "cluster", "--name", name)
		},
	}

	cmd.Flags().StringVar(&name, "name", internal.DefaultClusterName, "Cluster name")

	return cmd
}
