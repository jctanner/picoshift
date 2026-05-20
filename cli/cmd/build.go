package cmd

import (
	"fmt"
	"path/filepath"

	"github.com/jctanner/picoshift/internal"
	"github.com/spf13/cobra"
)

func NewBuildCmd() *cobra.Command {
	var kindOnly, simOnly bool

	cmd := &cobra.Command{
		Use:   "build",
		Short: "Build container images",
		Long:  "Build the kind CLI, base image, node image, and simulator image.",
		RunE: func(cmd *cobra.Command, args []string) error {
			root, err := internal.ProjectRoot()
			if err != nil {
				return err
			}
			if simOnly {
				return buildSim(root)
			}
			if kindOnly {
				return buildKind(root)
			}
			return buildAll(root)
		},
	}

	cmd.Flags().BoolVar(&kindOnly, "kind", false, "Build only kind CLI + images")
	cmd.Flags().BoolVar(&simOnly, "sim", false, "Build only simulator image")

	return cmd
}

func buildAll(root string) error {
	if err := buildKind(root); err != nil {
		return err
	}
	return buildSim(root)
}

func buildKind(root string) error {
	kindFork := filepath.Join(root, internal.KindForkDir)

	fmt.Println("[1/3] Building kind CLI...")
	if err := internal.Run("make", "-C", kindFork, "build"); err != nil {
		return err
	}

	fmt.Println("[2/3] Building kind base image (with ocp-shim)...")
	shimDir := filepath.Join(kindFork, "cmd/ocp-shim")
	baseDir := filepath.Join(kindFork, "images/base/ocp-shim")
	for _, f := range []string{"main.go", "go.mod", "go.sum"} {
		if err := internal.Run("cp", filepath.Join(shimDir, f), baseDir+"/"); err != nil {
			return err
		}
	}
	if err := internal.RunSudo(
		"podman", "build",
		"--build-arg", "GO_VERSION=1.26.2",
		"-t", internal.BaseImage,
		filepath.Join(kindFork, "images/base/"),
	); err != nil {
		return err
	}

	fmt.Println("[3/3] Building kind node image...")
	return internal.RunSudo(
		filepath.Join(root, internal.KindBin),
		"build", "node-image", internal.DefaultK8sVersion,
		"--type", "release",
		"--base-image", internal.BaseImage,
		"--image", internal.NodeImage,
	)
}

func buildSim(root string) error {
	fmt.Println("Building simulator image...")
	return internal.RunSudo(
		"podman", "build",
		"-t", internal.SimImage,
		filepath.Join(root, "simulator"),
	)
}
