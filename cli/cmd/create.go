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
		name       string
		authMode   string
		noDeploy   bool
		build      bool
		pullSecret string
		withOLM    bool
		withOSSM3  bool
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
			if build && !internal.IsDevMode() {
				fmt.Printf("picoshift %s uses pre-built images from %s — ignoring --build.\n",
					internal.Version, internal.DefaultRegistry)
				build = false
			}
			if withOSSM3 {
				if pullSecret == "" {
					return fmt.Errorf("--with-ossm3 requires --pull-secret")
				}
				withOLM = true
			}

			totalSteps := 8
			if authMode == "byoidc" {
				totalSteps += 2
			}
			if pullSecret != "" {
				totalSteps++
			}
			if withOLM {
				totalSteps++
			}
			if withOSSM3 {
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

			kindBin := internal.ResolvedKindBin(root)
			nodeImage := internal.ResolvedNodeImage()

			step++
			if internal.KindKnows(root, name) {
				fmt.Printf("[%d/%d] Cluster %q already exists\n", step, totalSteps, name)
			} else {
				fmt.Printf("[%d/%d] Creating kind cluster %q (image=%s)...\n", step, totalSteps, name, nodeImage)
				createArgs := []string{
					kindBin, "create", "cluster",
					"--config", filepath.Join(root, internal.KindConfig),
					"--image", nodeImage,
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

			if authMode == "byoidc" {
				step++
				fmt.Printf("[%d/%d] Patching kube-apiserver for BYOIDC...\n", step, totalSteps)
				if err := patchAPIServerOIDC(name); err != nil {
					return err
				}
			}

			step++
			fmt.Printf("[%d/%d] Setting up admin RBAC...\n", step, totalSteps)
			if err := internal.Run("python3", filepath.Join(root, "scripts/setup-admin-rbac.py")); err != nil {
				return err
			}

			if pullSecret != "" {
				step++
				fmt.Printf("[%d/%d] Storing pull secret...\n", step, totalSteps)
				if err := storePullSecret(pullSecret); err != nil {
					return err
				}
			}

			if withOLM {
				step++
				fmt.Printf("[%d/%d] Installing OLM...\n", step, totalSteps)
				if err := installOLM(root); err != nil {
					return err
				}
			}

			if withOSSM3 {
				step++
				fmt.Printf("[%d/%d] Installing OSSM3 gateway stack...\n", step, totalSteps)
				if err := installOSSM3(name, pullSecret); err != nil {
					return err
				}
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
	cmd.Flags().StringVar(&pullSecret, "pull-secret", "", "Path to Docker/Podman config.json with registry credentials")
	cmd.Flags().BoolVar(&withOLM, "with-olm", false, "Install OLM (Operator Lifecycle Manager)")
	cmd.Flags().BoolVar(&withOSSM3, "with-ossm3", false, "Install OSSM3 gateway stack (implies --with-olm, requires --pull-secret)")

	return cmd
}

func checkDeps(root string) error {
	kindBin := internal.ResolvedKindBin(root)
	if err := internal.CheckFile(kindBin); err != nil {
		if internal.IsDevMode() {
			fmt.Println("Kind binary not found. Run 'picoshift init' and 'picoshift build --kind' first.")
		} else {
			return fmt.Errorf("kind binary not found — place it next to picoshift (as 'kind' or 'kind-linux-amd64') or on PATH.\nDownload from: https://github.com/jctanner/picoshift/releases/tag/v%s", internal.Version)
		}
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
		if d == "olm" && isOLMInstalled() {
			fmt.Println("  OLM is running — skipping stub OLM CRDs")
			continue
		}
		if err := internal.Run("kubectl", "apply", "-f", filepath.Join(crdsDir, d)); err != nil {
			return err
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
	if !internal.IsDevMode() {
		entraMockImage := internal.ResolvedEntraMockImage()
		fmt.Printf("  Pulling %s...\n", entraMockImage)
		if err := internal.RunSudo("podman", "pull", entraMockImage); err != nil {
			return fmt.Errorf("failed to pull entra-mock image: %w", err)
		}
		if err := internal.RunSudo("podman", "tag", entraMockImage, internal.EntraMockImage); err != nil {
			return fmt.Errorf("failed to tag entra-mock image: %w", err)
		}
	}

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
	simImage := internal.ResolvedSimImage()

	if !internal.IsDevMode() {
		fmt.Printf("  Pulling %s...\n", simImage)
		if err := internal.RunSudo("podman", "pull", simImage); err != nil {
			return fmt.Errorf("failed to pull simulator image: %w", err)
		}
		if err := internal.RunSudo("podman", "tag", simImage, internal.SimImage); err != nil {
			return fmt.Errorf("failed to tag simulator image: %w", err)
		}
	}

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

func patchAPIServerOIDC(clusterName string) error {
	controlPlane := clusterName + "-control-plane"
	manifest := "/etc/kubernetes/manifests/kube-apiserver.yaml"

	fmt.Println("  Extracting OIDC CA cert from simulator proxy...")
	if err := internal.RunSudo("podman", "exec", controlPlane, "sh", "-c",
		`echo | openssl s_client -connect localhost:443 -servername entra.apps.ocp-sim.test 2>/dev/null `+
			`| openssl x509 -outform PEM > /etc/kubernetes/pki/oidc-ca.crt`,
	); err != nil {
		return fmt.Errorf("failed to extract OIDC CA cert: %w", err)
	}

	fmt.Println("  Patching kube-apiserver manifest with OIDC flags...")
	oidcFlags := fmt.Sprintf(
		`        - --oidc-issuer-url=%s\n`+
			`        - --oidc-client-id=picoshift\n`+
			`        - --oidc-username-claim=preferred_username\n`+
			`        - --oidc-groups-claim=groups\n`+
			`        - --oidc-username-prefix=-\n`+
			`        - --oidc-groups-prefix=\n`+
			`        - --oidc-ca-file=/etc/kubernetes/pki/oidc-ca.crt`,
		internal.ByoidcIssuerURL,
	)
	if err := internal.RunSudo("podman", "exec", controlPlane, "sh", "-c",
		fmt.Sprintf(
			`grep -q -- "--oidc-issuer-url=https://entra" %s && exit 0; `+
				`sed -i "/--secure-port=16443/a\%s" %s`,
			manifest, oidcFlags, manifest,
		),
	); err != nil {
		return fmt.Errorf("failed to patch kube-apiserver manifest: %w", err)
	}

	fmt.Println("  Waiting for kube-apiserver to restart...")
	time.Sleep(20 * time.Second)
	deadline := time.Now().Add(90 * time.Second)
	for time.Now().Before(deadline) {
		if err := internal.RunQuiet("kubectl", "get", "nodes", "--request-timeout=10s"); err == nil {
			break
		}
		time.Sleep(5 * time.Second)
	}
	if err := internal.Run("kubectl", "get", "nodes", "--request-timeout=60s"); err != nil {
		return fmt.Errorf("kube-apiserver did not recover after OIDC patch: %w", err)
	}

	fmt.Println("  kube-apiserver OIDC configuration complete")
	return nil
}

func storePullSecret(pullSecretPath string) error {
	_ = internal.RunQuiet("kubectl", "-n", internal.SimNamespace,
		"delete", "secret", internal.PullSecretName, "--ignore-not-found")
	return internal.Run("kubectl", "-n", internal.SimNamespace,
		"create", "secret", "docker-registry", internal.PullSecretName,
		fmt.Sprintf("--from-file=.dockerconfigjson=%s", pullSecretPath))
}

func installOLM(root string) error {
	if isOLMInstalled() {
		fmt.Println("  OLM is already installed")
		return nil
	}

	version := internal.OLMDefaultVersion
	base := olmBaseURL(version)

	fmt.Println("  Removing stub OLM CRDs...")
	_ = internal.RunQuiet("kubectl", "delete", "-f",
		filepath.Join(root, "deploy/crds/olm/"), "--ignore-not-found")

	fmt.Printf("  Installing OLM %s CRDs...\n", version)
	if err := internal.Run("kubectl", "create", "-f", base+"/crds.yaml"); err != nil {
		return fmt.Errorf("failed to install OLM CRDs: %w", err)
	}
	if err := internal.Run("kubectl", "wait", "--for=condition=Established",
		"crd", "--all", "--timeout=30s"); err != nil {
		return err
	}

	fmt.Printf("  Installing OLM %s controllers...\n", version)
	if err := internal.Run("kubectl", "create", "-f", base+"/olm.yaml"); err != nil {
		return fmt.Errorf("failed to install OLM controllers: %w", err)
	}

	fmt.Println("  Waiting for OLM rollout...")
	for _, deploy := range []string{"olm-operator", "catalog-operator"} {
		if err := internal.Run("kubectl", "rollout", "status",
			fmt.Sprintf("deployment/%s", deploy),
			"-n", internal.OLMNamespace, "--timeout=120s"); err != nil {
			return fmt.Errorf("%s failed to roll out: %w", deploy, err)
		}
	}

	fmt.Println("  Deploying community operators catalog...")
	if err := internal.Run("kubectl", "apply", "-f",
		filepath.Join(root, "deploy/olm/catalogsource.yaml")); err != nil {
		return fmt.Errorf("failed to deploy CatalogSource: %w", err)
	}
	if err := waitForCatalogSource("community-operators", 5*time.Minute); err != nil {
		return err
	}

	fmt.Println("  OLM installed")
	return nil
}

func installNodePullSecret(clusterName, pullSecretPath string) error {
	controlPlane := clusterName + "-control-plane"

	if err := internal.RunSudo("podman", "cp",
		pullSecretPath, controlPlane+":/tmp/pull-secret.json"); err != nil {
		return fmt.Errorf("failed to copy pull secret to node: %w", err)
	}

	// Generate hosts.toml for each registry using jq + shell on the node
	if err := internal.RunSudo("podman", "exec", controlPlane, "sh", "-c", `
for registry in $(jq -r '.auths | keys[]' /tmp/pull-secret.json); do
  auth=$(jq -r --arg r "$registry" '.auths[$r].auth // empty' /tmp/pull-secret.json)
  [ -z "$auth" ] && continue
  mkdir -p "/etc/containerd/certs.d/$registry"
  cat > "/etc/containerd/certs.d/$registry/hosts.toml" <<HOSTEOF
server = "https://$registry"

[host."https://$registry"]
  capabilities = ["pull", "resolve"]
  [host."https://$registry".header]
    Authorization = ["Basic $auth"]
HOSTEOF
done
`); err != nil {
		return fmt.Errorf("failed to generate containerd hosts config: %w", err)
	}

	if err := internal.RunSudo("podman", "exec", controlPlane, "sh", "-c",
		`grep -q 'config_path' /etc/containerd/config.toml || `+
			`sed -i '/\[plugins\."io\.containerd\.grpc\.v1\.cri"\.registry\]/a\  config_path = "/etc/containerd/certs.d"' /etc/containerd/config.toml || `+
			`printf '\n[plugins."io.containerd.grpc.v1.cri".registry]\n  config_path = "/etc/containerd/certs.d"\n' >> /etc/containerd/config.toml`,
	); err != nil {
		return fmt.Errorf("failed to configure containerd config_path: %w", err)
	}

	if err := internal.RunSudo("podman", "exec", controlPlane,
		"systemctl", "restart", "containerd"); err != nil {
		return fmt.Errorf("failed to restart containerd: %w", err)
	}

	time.Sleep(5 * time.Second)
	deadline := time.Now().Add(60 * time.Second)
	for time.Now().Before(deadline) {
		if err := internal.RunQuiet("kubectl", "get", "nodes", "--request-timeout=5s"); err == nil {
			return nil
		}
		time.Sleep(5 * time.Second)
	}
	return fmt.Errorf("node did not recover after containerd restart")
}

func installOSSM3(clusterName, pullSecretPath string) error {
	fmt.Println("  Installing node-level pull secret...")
	if err := installNodePullSecret(clusterName, pullSecretPath); err != nil {
		return err
	}

	fmt.Println("  Adding redhat-operators catalog...")
	secretName := "catalog-pull-redhat-operators"
	_ = internal.RunQuiet("kubectl", "-n", internal.OLMNamespace,
		"delete", "secret", secretName, "--ignore-not-found")
	if err := internal.Run("kubectl", "-n", internal.OLMNamespace,
		"create", "secret", "docker-registry", secretName,
		fmt.Sprintf("--from-file=.dockerconfigjson=%s", pullSecretPath),
	); err != nil {
		return fmt.Errorf("failed to create catalog pull secret: %w", err)
	}
	if err := applyCatalogSource("redhat-operators", internal.RedHatCatalogImage, secretName); err != nil {
		return err
	}
	if err := waitForCatalogSource("redhat-operators", 5*time.Minute); err != nil {
		return err
	}

	fmt.Println("  Installing Gateway API CRDs (v1 + v1beta1)...")
	if err := internal.Run("kubectl", "apply", "-f", internal.GatewayAPICRDsURL); err != nil {
		return fmt.Errorf("failed to install Gateway API CRDs: %w", err)
	}

	fmt.Println("  Removing stub Istio CRDs (conflict with servicemeshoperator3)...")
	_ = internal.RunQuiet("kubectl", "delete", "crd", "-l",
		"operators.coreos.com/sailoperator.openshift-operators", "--ignore-not-found")
	for _, crd := range []string{
		"istios.sailoperator.io",
		"istiocnis.sailoperator.io",
		"istiorevisions.sailoperator.io",
		"remoteistios.sailoperator.io",
		"ztunnels.sailoperator.io",
		"wasmplugins.extensions.istio.io",
		"telemetries.telemetry.istio.io",
	} {
		_ = internal.RunQuiet("kubectl", "delete", "crd", crd, "--ignore-not-found")
	}

	fmt.Println("  Installing servicemeshoperator3...")
	if err := ensureOperatorGroup("openshift-operators"); err != nil {
		return err
	}
	if err := createSubscription("servicemeshoperator3", "openshift-operators",
		"stable-3.0", "redhat-operators"); err != nil {
		return err
	}
	if err := waitForCSV("servicemeshoperator3", "openshift-operators", 5*time.Minute); err != nil {
		return err
	}

	fmt.Println("  Creating istio-system namespace and Istio CR...")
	_ = internal.Run("kubectl", "create", "namespace", internal.IstioNamespace)

	if err := applyIstioCR(); err != nil {
		return err
	}

	fmt.Println("  Patching API server audiences for istio-ca...")
	if err := patchAPIServerAudiences(clusterName); err != nil {
		return err
	}

	fmt.Println("  Waiting for istiod...")
	if err := waitForIstiod(5 * time.Minute); err != nil {
		return err
	}

	fmt.Println("  OSSM3 gateway stack installed")
	return nil
}

func applyIstioCR() error {
	cr := `apiVersion: sailoperator.io/v1
kind: Istio
metadata:
  name: openshift-gateway
spec:
  namespace: istio-system
  updateStrategy:
    type: InPlace
  values:
    global:
      istioNamespace: istio-system
      priorityClassName: system-cluster-critical
    pilot:
      enabled: true
      env:
        PILOT_ENABLE_GATEWAY_API: "true"
        PILOT_ENABLE_ALPHA_GATEWAY_API: "false"
        PILOT_ENABLE_GATEWAY_API_STATUS: "true"
        PILOT_ENABLE_GATEWAY_API_DEPLOYMENT_CONTROLLER: "true"
        PILOT_ENABLE_GATEWAY_API_GATEWAYCLASS_CONTROLLER: "false"
        PILOT_GATEWAY_API_DEFAULT_GATEWAYCLASS_NAME: "openshift-default"
        PILOT_GATEWAY_API_CONTROLLER_NAME: "openshift.io/gateway-controller/v1"
        PILOT_MULTI_NETWORK_DISCOVER_GATEWAY_API: "false"
        ENABLE_GATEWAY_API_MANUAL_DEPLOYMENT: "false"
        PILOT_ENABLE_GATEWAY_API_CA_CERT_ONLY: "true"
        PILOT_ENABLE_GATEWAY_API_COPY_LABELS_ANNOTATIONS: "false"`

	return internal.Run("bash", "-c",
		fmt.Sprintf("cat <<'EOF' | kubectl apply -f -\n%s\nEOF", cr))
}

func patchAPIServerAudiences(clusterName string) error {
	controlPlane := clusterName + "-control-plane"
	manifest := "/etc/kubernetes/manifests/kube-apiserver.yaml"
	return internal.RunSudo("podman", "exec", controlPlane, "sh", "-c",
		fmt.Sprintf(
			`grep -q -- "--api-audiences=.*istio-ca" %s && exit 0; `+
				`sed -i '/--service-account-issuer=/a\        - --api-audiences=https://kubernetes.default.svc.cluster.local,istio-ca' %s`,
			manifest, manifest,
		),
	)
}

func waitForIstiod(timeout time.Duration) error {
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		out, err := internal.RunOutputQuiet("kubectl", "get", "istio", "openshift-gateway",
			"-o", "jsonpath={.status.state}")
		if err == nil && out == "Healthy" {
			fmt.Println("  istiod is healthy")
			return nil
		}
		time.Sleep(10 * time.Second)
	}
	return fmt.Errorf("istiod did not become healthy after %v", timeout)
}
