package cmd

import (
	"fmt"
	"path/filepath"
	"strings"
	"time"

	"github.com/jctanner/picoshift/internal"
	"github.com/spf13/cobra"
)

func NewCreateCmd(version string) *cobra.Command {
	var (
		name     string
		authMode string
		noDeploy bool
		build    bool
	)

	cmd := &cobra.Command{
		Use:   "create",
		Short: "Create cluster and deploy the simulator",
		Long: `Create a kind cluster with the OpenShift simulator deployed.

Creates the cluster, installs CRDs and seed resources, and deploys the
simulator using existing images. Use --build to build images first, or
run 'picoshift build' separately.`,
		RunE: func(cmd *cobra.Command, args []string) error {
			root, err := internal.ProjectRoot()
			if err != nil {
				return err
			}

			if authMode != "legacy" && authMode != "oidc" && authMode != "byoidc" {
				return fmt.Errorf("invalid --auth-mode %q: must be legacy, oidc, or byoidc", authMode)
			}

			totalSteps := 8
			if authMode == "byoidc" {
				totalSteps++
			}
			if build {
				totalSteps += 4
				if authMode == "byoidc" {
					totalSteps++
				}
			}
			if noDeploy {
				totalSteps = 2
				if build {
					totalSteps = 6
					if authMode == "byoidc" {
						totalSteps++
					}
				}
			}

			step := 0

			if err := checkDeps(root); err != nil {
				return err
			}

			if build {
				step++
				fmt.Printf("[%d/%d] Building kind CLI...\n", step, totalSteps)
				if err := internal.Run("make", "-C", filepath.Join(root, internal.KindForkDir), "build"); err != nil {
					return err
				}

				step++
				fmt.Printf("[%d/%d] Building kind base image...\n", step, totalSteps)
				kindFork := filepath.Join(root, internal.KindForkDir)
				shimDir := filepath.Join(kindFork, "cmd/ocp-shim")
				baseDir := filepath.Join(kindFork, "images/base/ocp-shim")
				for _, f := range []string{"main.go", "go.mod", "go.sum"} {
					if err := internal.Run("cp", filepath.Join(shimDir, f), baseDir+"/"); err != nil {
						return err
					}
				}
				if err := internal.RunSudo(
					"podman", "build", "--build-arg", "GO_VERSION=1.26.2",
					"-t", internal.BaseImage, filepath.Join(kindFork, "images/base/"),
				); err != nil {
					return err
				}

				step++
				fmt.Printf("[%d/%d] Building kind node image...\n", step, totalSteps)
				if err := internal.RunSudo(
					filepath.Join(root, internal.KindBin),
					"build", "node-image", internal.DefaultK8sVersion,
					"--type", "release",
					"--base-image", internal.BaseImage,
					"--image", internal.NodeImage,
				); err != nil {
					return err
				}

				step++
				fmt.Printf("[%d/%d] Building simulator image...\n", step, totalSteps)
				if err := internal.RunSudo(
					"podman", "build", "-t", internal.SimImage,
					filepath.Join(root, "simulator"),
				); err != nil {
					return err
				}

				if authMode == "byoidc" {
					step++
					fmt.Printf("[%d/%d] Building entra-mock image...\n", step, totalSteps)
					if err := buildEntraMock(root); err != nil {
						return err
					}
				}
			}

			kindBin := filepath.Join(root, internal.KindBin)

			step++
			if internal.KindKnows(root, name) {
				fmt.Printf("[%d/%d] Cluster %q already exists\n", step, totalSteps, name)
			} else {
				fmt.Printf("[%d/%d] Creating kind cluster %q...\n", step, totalSteps, name)
				createArgs := []string{
					kindBin, "create", "cluster",
					"--config", filepath.Join(root, internal.KindConfig),
					"--image", internal.NodeImage,
					"--name", name,
				}
				maxRetries := 3
				for attempt := 1; attempt <= maxRetries; attempt++ {
					err := internal.RunSudo(createArgs...)
					if err == nil {
						break
					}
					if attempt == maxRetries {
						return fmt.Errorf("kind create failed after %d attempts: %w", maxRetries, err)
					}
					fmt.Printf("  cluster creation failed (attempt %d/%d), retrying...\n", attempt, maxRetries)
					time.Sleep(5 * time.Second)
				}
			}

			step++
			fmt.Printf("[%d/%d] Exporting kubeconfig...\n", step, totalSteps)
			if err := internal.ExportKubeconfig(root, name); err != nil {
				return err
			}

			if noDeploy {
				fmt.Println("\n=== Cluster created (--no-deploy: skipping CRDs, seed, simulator) ===")
				return nil
			}

			step++
			fmt.Printf("[%d/%d] Waiting for node...\n", step, totalSteps)
			if err := internal.WaitForNode(120 * time.Second); err != nil {
				return err
			}

			step++
			fmt.Printf("[%d/%d] Deploying CRDs...\n", step, totalSteps)
			if err := deployCRDs(root); err != nil {
				return err
			}

			step++
			fmt.Printf("[%d/%d] Deploying seed resources (auth-mode=%s)...\n", step, totalSteps, authMode)
			if err := deploySeed(root, authMode); err != nil {
				return err
			}

			step++
			fmt.Printf("[%d/%d] Creating ClusterVersion...\n", step, totalSteps)
			if err := deployClusterVersion(); err != nil {
				return err
			}

			if authMode == "byoidc" {
				step++
				fmt.Printf("[%d/%d] Deploying entra-mock...\n", step, totalSteps)
				if err := deployEntraMock(root, name); err != nil {
					return err
				}
			}

			step++
			fmt.Printf("[%d/%d] Deploying simulator...\n", step, totalSteps)
			if err := deploySim(root, name, authMode); err != nil {
				return err
			}

			step++
			fmt.Printf("[%d/%d] Setting up admin RBAC...\n", step, totalSteps)
			if err := internal.Run("python3", filepath.Join(root, "scripts/setup-admin-rbac.py")); err != nil {
				return err
			}

			fmt.Println("\n=== Ready ===")
			fmt.Println("  picoshift status")
			fmt.Println("  picoshift logs")
			fmt.Println("  https://rh-ai.apps.ocp-sim.test/")
			return nil
		},
	}

	cmd.Flags().StringVar(&name, "name", internal.DefaultClusterName, "Cluster name")
	cmd.Flags().StringVar(&authMode, "auth-mode", internal.DefaultAuthMode, "Auth mode: legacy, oidc, or byoidc")
	cmd.Flags().BoolVar(&noDeploy, "no-deploy", false, "Create cluster only, skip CRDs/seed/simulator")
	cmd.Flags().BoolVar(&build, "build", false, "Build all images before creating the cluster")

	return cmd
}

func checkDeps(root string) error {
	if err := internal.CheckFile(filepath.Join(root, internal.KindBin)); err != nil {
		fmt.Println("Kind binary not found. Run 'picoshift init' and 'picoshift build --kind' first.")
	}
	for _, dep := range []string{"kubectl", "podman"} {
		if err := internal.CheckDep(dep); err != nil {
			return err
		}
	}
	return nil
}

func deployCRDs(root string) error {
	crdsDir := filepath.Join(root, "deploy/crds")
	dirs := []string{"openshift", "olm", "gateway", "monitoring", "istio", "authorino", "kuadrant"}
	for _, d := range dirs {
		if d == "jobset" {
			if err := internal.Run("kubectl", "apply", "--server-side", "-f", filepath.Join(crdsDir, d)); err != nil {
				return err
			}
		} else {
			if err := internal.Run("kubectl", "apply", "-f", filepath.Join(crdsDir, d)); err != nil {
				return err
			}
		}
	}
	if err := internal.Run("kubectl", "apply", "--server-side", "-f", filepath.Join(crdsDir, "jobset")); err != nil {
		return err
	}
	return internal.Run("kubectl", "wait", "--for=condition=Established", "crd", "--all", "--timeout=30s")
}

func deploySeed(root, authMode string) error {
	seedDir := filepath.Join(root, "deploy/seed")
	files := []string{
		"namespaces.yaml",
		"cluster-config.yaml",
		"authentication.yaml",
	}
	if authMode != "byoidc" {
		files = append(files, "htpasswd.yaml")
	}
	if authMode != "legacy" {
		files = append(files, "authentication-oidc.yaml")
	}
	files = append(files,
		"ingress.yaml",
		"infrastructure.yaml",
		"sccs.yaml",
		"jobset-operator.yaml",
		"rbac-compat.yaml",
	)
	for _, f := range files {
		if err := internal.Run("kubectl", "apply", "-f", filepath.Join(seedDir, f)); err != nil {
			return err
		}
	}

	// Patch kubernetes endpoints to route through ocp-shim
	_ = internal.Run("kubectl", "patch", "endpoints", "kubernetes", "-n", "default",
		"--type=json",
		`-p=[{"op":"replace","path":"/subsets/0/ports/0/port","value":6443}]`)

	return nil
}

func deployClusterVersion() error {
	spec := `{"apiVersion":"config.openshift.io/v1","kind":"ClusterVersion","metadata":{"name":"version"},"spec":{"clusterID":"ocp-sim-00000000-0000-0000-0000-000000000000","channel":"stable-4.20"}}`
	if err := internal.Run("bash", "-c", fmt.Sprintf("echo '%s' | kubectl apply -f -", spec)); err != nil {
		return err
	}

	script := `
kubectl proxy --port=8199 &
PROXY_PID=$!
sleep 1
RV=$(curl -s http://localhost:8199/apis/config.openshift.io/v1/clusterversions/version | python3 -c "import sys,json; print(json.load(sys.stdin)['metadata']['resourceVersion'])")
curl -s -X PUT http://localhost:8199/apis/config.openshift.io/v1/clusterversions/version/status \
  -H "Content-Type: application/json" \
  -d "{\"apiVersion\":\"config.openshift.io/v1\",\"kind\":\"ClusterVersion\",\"metadata\":{\"name\":\"version\",\"resourceVersion\":\"${RV}\"},\"spec\":{\"clusterID\":\"ocp-sim-00000000-0000-0000-0000-000000000000\",\"channel\":\"stable-4.20\"},\"status\":{\"desired\":{\"version\":\"4.20.0\"},\"history\":[{\"state\":\"Completed\",\"version\":\"4.20.0\",\"startedTime\":\"2024-01-01T00:00:00Z\",\"completionTime\":\"2024-01-01T01:00:00Z\",\"verified\":true}],\"conditions\":[{\"type\":\"Available\",\"status\":\"True\",\"lastTransitionTime\":\"2024-01-01T01:00:00Z\",\"reason\":\"ClusterVersionAvailable\",\"message\":\"Simulated OCP cluster\"},{\"type\":\"Progressing\",\"status\":\"False\",\"lastTransitionTime\":\"2024-01-01T01:00:00Z\",\"reason\":\"ClusterVersionNotProgressing\"},{\"type\":\"Failing\",\"status\":\"False\",\"lastTransitionTime\":\"2024-01-01T01:00:00Z\",\"reason\":\"ClusterVersionNotFailing\"}]}}" > /dev/null
kill $PROXY_PID 2>/dev/null; wait $PROXY_PID 2>/dev/null || true
`
	return internal.Run("bash", "-c", script)
}

func deployEntraMock(root, clusterName string) error {
	if err := internal.RunSudo(
		"podman", "save", internal.EntraMockImage,
		"--format", "oci-archive", "-o", "/tmp/entra-mock-oci.tar",
	); err != nil {
		return err
	}
	if err := internal.Run("bash", "-c", fmt.Sprintf(
		"%s podman exec -i %s-control-plane ctr --namespace=k8s.io images import --no-unpack - < /tmp/entra-mock-oci.tar",
		internal.Sudo(), clusterName,
	)); err != nil {
		return err
	}
	_ = internal.RunSudo("rm", "-f", "/tmp/entra-mock-oci.tar")

	if err := internal.Run("kubectl", "apply", "-f", filepath.Join(root, "deploy/entra-mock.yaml")); err != nil {
		return err
	}
	return internal.Run("kubectl", "-n", internal.EntraMockNamespace,
		"rollout", "status", "deployment/entra-mock", "--timeout=120s")
}

func deploySim(root, clusterName, authMode string) error {
	// Load image into cluster
	if err := internal.RunSudo(
		"podman", "save", internal.SimImage,
		"--format", "oci-archive", "-o", "/tmp/ocp-sim-oci.tar",
	); err != nil {
		return err
	}
	if err := internal.Run("bash", "-c", fmt.Sprintf(
		"%s podman exec -i %s-control-plane ctr --namespace=k8s.io images import --no-unpack - < /tmp/ocp-sim-oci.tar",
		internal.Sudo(), clusterName,
	)); err != nil {
		return err
	}
	_ = internal.RunSudo("rm", "-f", "/tmp/ocp-sim-oci.tar")

	// Create namespace and configmap
	_ = internal.Run("kubectl", "create", "namespace", internal.SimNamespace)
	if err := internal.Run("bash", "-c", fmt.Sprintf(
		"kubectl -n %s create configmap ocp-sim-users --from-file=%s --dry-run=client -o yaml | kubectl apply -f -",
		internal.SimNamespace, filepath.Join(root, internal.UsersFile),
	)); err != nil {
		return err
	}

	// Apply simulator manifest
	if err := internal.Run("kubectl", "apply", "-f", filepath.Join(root, internal.SimManifest)); err != nil {
		return err
	}

	// Patch auth mode if not legacy
	if authMode == "oidc" {
		args := `["--proxy","--proxy-port","80","--users-file","/etc/ocp-sim/users.yaml","--auth-mode","oidc"]`
		if err := internal.Run("kubectl", "-n", internal.SimNamespace,
			"patch", "daemonset", "ocp-sim", "--type=json",
			fmt.Sprintf(`-p=[{"op":"replace","path":"/spec/template/spec/containers/0/args","value":%s}]`, args),
		); err != nil {
			return err
		}
	} else if authMode == "byoidc" {
		args := strings.Join([]string{
			`["--proxy","--proxy-port","80","--users-file","/etc/ocp-sim/users.yaml"`,
			`,"--auth-mode","byoidc"`,
			`,"--oidc-issuer-url","http://entra-mock.entra-mock.svc.cluster.local:8080/a1b2c3d4-e5f6-7890-abcd-ef1234567890/v2.0"`,
			`,"--oidc-client-id","picoshift"`,
			`,"--oidc-client-secret","picoshift-secret"]`,
		}, "")
		if err := internal.Run("kubectl", "-n", internal.SimNamespace,
			"patch", "daemonset", "ocp-sim", "--type=json",
			fmt.Sprintf(`-p=[{"op":"replace","path":"/spec/template/spec/containers/0/args","value":%s}]`, args),
		); err != nil {
			return err
		}
	}

	// Restart pods and wait
	_ = internal.Run("kubectl", "-n", internal.SimNamespace, "delete", "pod", "--all", "--wait=false")
	time.Sleep(3 * time.Second)
	return internal.Run("kubectl", "wait", "--namespace", internal.SimNamespace,
		"--for=condition=Ready", "pod", "--selector=app=ocp-sim", "--timeout=120s")
}
