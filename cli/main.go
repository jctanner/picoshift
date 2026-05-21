package main

import (
	"fmt"
	"os"

	"github.com/jctanner/picoshift/cmd"
	"github.com/jctanner/picoshift/internal"
	"github.com/spf13/cobra"
)

var Version = "dev"

func main() {
	rootCmd := &cobra.Command{
		Use:   "picoshift",
		Short: "Lightweight OpenShift simulator on kind",
		Long: `picoshift manages the lifecycle of a lightweight OpenShift simulator
running on a kind cluster. It replaces the Makefile for cluster creation,
deployment, and management.`,
		SilenceUsage: true,
		PersistentPreRun: func(c *cobra.Command, args []string) {
			dryRun, _ := c.Flags().GetBool("dry-run")
			internal.DryRun = dryRun
		},
	}

	rootCmd.PersistentFlags().Bool("dry-run", false, "Print commands instead of executing them")

	rootCmd.AddCommand(
		cmd.NewInitCmd(),
		cmd.NewBuildCmd(),
		cmd.NewCreateCmd(Version),
		cmd.NewDeleteCmd(),
		cmd.NewStatusCmd(),
		cmd.NewLogsCmd(),
		cmd.NewUserCmd(),
		cmd.NewOlmCmd(),
		cmd.NewVersionCmd(Version),
	)

	if err := rootCmd.Execute(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
