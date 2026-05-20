package cmd

import (
	"github.com/jctanner/picoshift/internal"
	"github.com/spf13/cobra"
)

func NewLogsCmd() *cobra.Command {
	var operator bool

	cmd := &cobra.Command{
		Use:   "logs",
		Short: "Tail simulator or operator logs",
		RunE: func(cmd *cobra.Command, args []string) error {
			if operator {
				return internal.Run("kubectl", "-n", internal.OperatorNamespace,
					"logs", "-l", "control-plane=controller-manager",
					"-c", "manager", "-f")
			}
			return internal.Run("kubectl", "-n", internal.SimNamespace,
				"logs", "-l", "app=ocp-sim", "-f")
		},
	}

	cmd.Flags().BoolVar(&operator, "operator", false, "Tail operator logs instead of simulator")

	return cmd
}
