package cmd

import (
	"github.com/jctanner/picoshift/internal"
	"github.com/spf13/cobra"
)

func NewLogsCmd() *cobra.Command {
	var (
		operator bool
		name     string
	)

	cmd := &cobra.Command{
		Use:   "logs",
		Short: "Tail simulator or operator logs",
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx := resolveContext(name)

			if operator {
				a := kubectlWithCtx(ctx, "-n", internal.OperatorNamespace,
					"logs", "-l", "control-plane=controller-manager",
					"-c", "manager", "-f")
				return internal.Run("kubectl", a...)
			}

			a := kubectlWithCtx(ctx, "-n", internal.SimNamespace,
				"logs", "-l", "app=ocp-sim", "-f")
			return internal.Run("kubectl", a...)
		},
	}

	cmd.Flags().BoolVar(&operator, "operator", false, "Tail operator logs instead of simulator")
	cmd.Flags().StringVar(&name, "name", internal.DefaultClusterName, "Cluster name")

	return cmd
}
