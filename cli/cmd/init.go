package cmd

import (
	"fmt"

	"github.com/jctanner/picoshift/internal"
	"github.com/spf13/cobra"
)

func NewInitCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "init",
		Short: "Clone or update required dependencies",
		Long:  "Clones the kind fork, opendatahub-operator, and entra-id-emulator into deps/.",
		RunE: func(cmd *cobra.Command, args []string) error {
			root, err := internal.ProjectRoot()
			if err != nil {
				return err
			}
			fmt.Println("=== Initializing dependencies ===")
			return internal.Run("bash", root+"/scripts/init-deps.sh")
		},
	}
}
