package cmd

import (
	"fmt"
	"path/filepath"
	"strings"
	"time"

	"github.com/jctanner/picoshift/internal"
	"github.com/spf13/cobra"
)

func olmBaseURL(version string) string {
	return fmt.Sprintf(
		"https://github.com/operator-framework/operator-lifecycle-manager/releases/download/%s",
		version,
	)
}

func isOLMInstalled() bool {
	return internal.RunQuiet("kubectl", "get", "deployment", "olm-operator",
		"-n", internal.OLMNamespace) == nil
}

func NewOlmCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "olm",
		Short: "Manage OLM (Operator Lifecycle Manager)",
		Long: `Install, uninstall, and manage OLM and operators from the
community operators catalog (OperatorHub.io).`,
	}

	cmd.AddCommand(
		newOlmInstallCmd(),
		newOlmUninstallCmd(),
		newOlmOperatorCmd(),
	)
	return cmd
}

// --- olm install ---

func newOlmInstallCmd() *cobra.Command {
	var version string

	cmd := &cobra.Command{
		Use:   "install",
		Short: "Install OLM onto the cluster",
		RunE: func(cmd *cobra.Command, args []string) error {
			root, err := internal.ProjectRoot()
			if err != nil {
				return err
			}

			if isOLMInstalled() {
				fmt.Println("OLM is already installed")
				return nil
			}

			base := olmBaseURL(version)

			fmt.Println("[1/5] Removing stub OLM CRDs...")
			_ = internal.RunQuiet("kubectl", "delete", "-f",
				filepath.Join(root, "deploy/crds/olm/"), "--ignore-not-found")

			fmt.Printf("[2/5] Installing OLM %s CRDs...\n", version)
			if err := internal.Run("kubectl", "create", "-f", base+"/crds.yaml"); err != nil {
				return fmt.Errorf("failed to install OLM CRDs: %w", err)
			}
			if err := internal.Run("kubectl", "wait", "--for=condition=Established",
				"crd", "--all", "--timeout=30s"); err != nil {
				return err
			}

			fmt.Printf("[3/5] Installing OLM %s controllers...\n", version)
			if err := internal.Run("kubectl", "create", "-f", base+"/olm.yaml"); err != nil {
				return fmt.Errorf("failed to install OLM controllers: %w", err)
			}

			fmt.Println("[4/5] Waiting for OLM rollout...")
			for _, deploy := range []string{"olm-operator", "catalog-operator"} {
				if err := internal.Run("kubectl", "rollout", "status",
					fmt.Sprintf("deployment/%s", deploy),
					"-n", internal.OLMNamespace, "--timeout=120s"); err != nil {
					return fmt.Errorf("%s failed to roll out: %w", deploy, err)
				}
			}

			fmt.Println("[5/5] Deploying community operators catalog...")
			if err := internal.Run("kubectl", "apply", "-f",
				filepath.Join(root, "deploy/olm/catalogsource.yaml")); err != nil {
				return fmt.Errorf("failed to deploy CatalogSource: %w", err)
			}
			if err := waitForCatalogSource("community-operators", 5*time.Minute); err != nil {
				return err
			}

			fmt.Printf("\nOLM %s installed with community operators catalog\n", version)
			fmt.Println("  picoshift olm operator list")
			fmt.Println("  picoshift olm operator install <name>")
			return nil
		},
	}

	cmd.Flags().StringVar(&version, "version", internal.OLMDefaultVersion,
		"OLM version to install")
	return cmd
}

// --- olm uninstall ---

func newOlmUninstallCmd() *cobra.Command {
	var version string

	cmd := &cobra.Command{
		Use:   "uninstall",
		Short: "Remove OLM from the cluster and restore stub CRDs",
		RunE: func(cmd *cobra.Command, args []string) error {
			root, err := internal.ProjectRoot()
			if err != nil {
				return err
			}

			base := olmBaseURL(version)

			fmt.Println("[1/3] Removing OLM controllers...")
			_ = internal.RunQuiet("kubectl", "delete", "-f", base+"/olm.yaml",
				"--ignore-not-found")

			fmt.Println("[2/3] Removing OLM CRDs and namespace...")
			_ = internal.RunQuiet("kubectl", "delete", "-f", base+"/crds.yaml",
				"--ignore-not-found")
			_ = internal.RunQuiet("kubectl", "delete", "namespace",
				internal.OLMNamespace, "--ignore-not-found")

			fmt.Println("[3/3] Restoring stub OLM CRDs...")
			if err := internal.Run("kubectl", "apply", "-f",
				filepath.Join(root, "deploy/crds/olm/")); err != nil {
				return fmt.Errorf("failed to restore stub CRDs: %w", err)
			}

			fmt.Println("OLM uninstalled, stub CRDs restored")
			return nil
		},
	}

	cmd.Flags().StringVar(&version, "version", internal.OLMDefaultVersion,
		"OLM version that was installed")
	return cmd
}

// --- olm operator ---

func newOlmOperatorCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "operator",
		Short: "Manage operators via OLM",
	}
	cmd.AddCommand(
		newOlmOperatorListCmd(),
		newOlmOperatorInstallCmd(),
	)
	return cmd
}

func newOlmOperatorListCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "list",
		Short: "List available operators from the catalog",
		RunE: func(cmd *cobra.Command, args []string) error {
			if !isOLMInstalled() {
				return fmt.Errorf("OLM is not installed — run 'picoshift olm install' first")
			}
			return internal.Run("kubectl", "get", "packagemanifests",
				"-o", "custom-columns=NAME:.metadata.name,CATALOG:.status.catalogSource,CHANNEL:.status.defaultChannel",
				"--sort-by=.metadata.name")
		},
	}
}

func newOlmOperatorInstallCmd() *cobra.Command {
	var (
		channel   string
		namespace string
	)

	cmd := &cobra.Command{
		Use:   "install <operator-name>",
		Short: "Install an operator from the catalog",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			operatorName := args[0]

			if !isOLMInstalled() {
				return fmt.Errorf("OLM is not installed — run 'picoshift olm install' first")
			}

			if channel == "" {
				ch, err := getDefaultChannel(operatorName)
				if err != nil {
					return fmt.Errorf("operator %q not found in catalog: %w", operatorName, err)
				}
				channel = ch
				fmt.Printf("Using default channel: %s\n", channel)
			}

			catalogSource, err := getCatalogSource(operatorName)
			if err != nil {
				return fmt.Errorf("could not determine catalog source for %q: %w", operatorName, err)
			}

			fmt.Printf("[1/3] Ensuring OperatorGroup in %s...\n", namespace)
			if err := ensureOperatorGroup(namespace); err != nil {
				return err
			}

			fmt.Printf("[2/3] Creating Subscription for %s (channel=%s)...\n", operatorName, channel)
			if err := createSubscription(operatorName, namespace, channel, catalogSource); err != nil {
				return err
			}

			fmt.Println("[3/3] Waiting for CSV to succeed...")
			if err := waitForCSV(operatorName, namespace, 5*time.Minute); err != nil {
				return err
			}

			fmt.Printf("\nOperator %q installed successfully\n", operatorName)
			return nil
		},
	}

	cmd.Flags().StringVar(&channel, "channel", "", "Subscription channel (default: operator's default channel)")
	cmd.Flags().StringVar(&namespace, "namespace", "openshift-operators",
		"Namespace to install the operator in")
	return cmd
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

func waitForCatalogSource(name string, timeout time.Duration) error {
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		out, err := internal.RunOutputQuiet("kubectl", "get", "catalogsource", name,
			"-n", internal.OLMNamespace,
			"-o", "jsonpath={.status.connectionState.lastObservedState}")
		if err == nil && out == "READY" {
			fmt.Printf("CatalogSource %s is ready\n", name)
			return nil
		}
		time.Sleep(5 * time.Second)
	}
	return fmt.Errorf("CatalogSource %s not ready after %v", name, timeout)
}

func getDefaultChannel(operatorName string) (string, error) {
	out, err := internal.RunOutputQuiet("kubectl", "get", "packagemanifest", operatorName,
		"-o", "jsonpath={.status.defaultChannel}")
	if err != nil {
		return "", err
	}
	if out == "" {
		return "", fmt.Errorf("no default channel found")
	}
	return out, nil
}

func getCatalogSource(operatorName string) (string, error) {
	out, err := internal.RunOutputQuiet("kubectl", "get", "packagemanifest", operatorName,
		"-o", "jsonpath={.status.catalogSource}")
	if err != nil {
		return "", err
	}
	if out == "" {
		return "community-operators", nil
	}
	return out, nil
}

func ensureOperatorGroup(namespace string) error {
	_ = internal.RunQuiet("kubectl", "create", "namespace", namespace)

	out, err := internal.RunOutputQuiet("kubectl", "get", "operatorgroup",
		"-n", namespace, "-o", "jsonpath={.items[*].metadata.name}")
	if err == nil && strings.TrimSpace(out) != "" {
		return nil
	}

	og := fmt.Sprintf(`apiVersion: operators.coreos.com/v1
kind: OperatorGroup
metadata:
  name: og-%s
  namespace: %s`, namespace, namespace)

	return internal.Run("bash", "-c",
		fmt.Sprintf("cat <<'EOF' | kubectl apply -f -\n%s\nEOF", og))
}

func createSubscription(operatorName, namespace, channel, catalogSource string) error {
	sub := fmt.Sprintf(`apiVersion: operators.coreos.com/v1alpha1
kind: Subscription
metadata:
  name: %s
  namespace: %s
spec:
  channel: %s
  name: %s
  source: %s
  sourceNamespace: %s
  installPlanApproval: Automatic`,
		operatorName, namespace, channel, operatorName,
		catalogSource, internal.OLMNamespace)

	return internal.Run("bash", "-c",
		fmt.Sprintf("cat <<'EOF' | kubectl apply -f -\n%s\nEOF", sub))
}

func waitForCSV(operatorName, namespace string, timeout time.Duration) error {
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		out, err := internal.RunOutputQuiet("kubectl", "get", "csv",
			"-n", namespace,
			"-o", "jsonpath={range .items[*]}{.metadata.name}={.status.phase}\n{end}")
		if err == nil {
			for _, line := range strings.Split(out, "\n") {
				if strings.Contains(line, operatorName) && strings.HasSuffix(line, "=Succeeded") {
					csvName := strings.SplitN(line, "=", 2)[0]
					fmt.Printf("CSV %s succeeded\n", csvName)
					return nil
				}
			}
		}
		time.Sleep(5 * time.Second)
	}
	return fmt.Errorf("CSV for %s did not reach Succeeded after %v", operatorName, timeout)
}
