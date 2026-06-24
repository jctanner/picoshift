package internal

import (
	"fmt"
	"io/fs"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
)

var (
	Version        string
	EmbeddedAssets fs.FS
)

const (
	DefaultClusterName = "ocp-sim"
	DefaultK8sVersion  = "v1.33.1"
	DefaultAuthMode    = "legacy"

	DefaultRegistry = "ghcr.io/jctanner/picoshift"

	KindForkDir = "deps/kind"
	KindBin     = "deps/kind/bin/kind"
	BaseImage   = "kindest/base:ocp-shim"
	NodeImage   = "localhost/kindest/node:ocp-shim"
	SimImage       = "localhost/ocp-sim:latest"
	EntraMockImage = "localhost/entra-mock:latest"

	KindConfig  = "deploy/kind/cluster.yaml"
	SimManifest          = "deploy/simulator.yaml"
	SimNamespaceManifest = "deploy/simulator-namespace.yaml"
	UsersFile            = "deploy/users.yaml"

	SimNamespace       = "ocp-sim"
	OperatorNamespace  = "opendatahub-operator-system"
	IstioNamespace     = "istio-system"
	EntraMockNamespace = "entra-mock"

	HtpasswdSecret    = "htpass-secret"
	HtpasswdNamespace = "openshift-config"
	EntraMockAdminPass = "changeme1234"
	EntraMockTenantID  = "a1b2c3d4-e5f6-7890-abcd-ef1234567890"

	ByoidcIssuerURL = "https://entra.apps.ocp-sim.test/" + EntraMockTenantID + "/v2.0"

	PullSecretName     = "rh-pull-secret"
	RedHatCatalogImage = "registry.redhat.io/redhat/redhat-operator-index:v4.17"
	GatewayAPICRDsURL  = "https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.3.0/standard-install.yaml"

	OLMDefaultVersion = "v0.42.0"
	OLMNamespace      = "olm"

	ModeKind      = "kind"
	ModeNamespace = "namespace"
	DefaultMode   = ModeKind
)

var CRDDirs = []string{"openshift", "olm", "gateway", "monitoring", "istio", "authorino", "kuadrant"}

func IsDevMode() bool {
	return Version == "" || Version == "dev"
}

func versionTag() string {
	return strings.TrimPrefix(Version, "v")
}

func ResolvedSimImage() string {
	if IsDevMode() {
		return SimImage
	}
	return DefaultRegistry + "/ocp-sim:" + versionTag()
}

func ResolvedNodeImage() string {
	if IsDevMode() {
		return NodeImage
	}
	return DefaultRegistry + "/kindest-node:" + versionTag()
}

func ResolvedEntraMockImage() string {
	if IsDevMode() {
		return EntraMockImage
	}
	return DefaultRegistry + "/entra-mock:" + versionTag()
}

func ResolvedKindBin(root string) string {
	if IsDevMode() {
		return filepath.Join(root, KindBin)
	}
	// In release mode: look next to the picoshift binary first, then PATH
	if exe, err := os.Executable(); err == nil {
		dir := filepath.Dir(exe)
		for _, name := range []string{"kind-linux-amd64", "kind"} {
			candidate := filepath.Join(dir, name)
			if _, err := os.Stat(candidate); err == nil {
				return candidate
			}
		}
	}
	if path, err := exec.LookPath("kind"); err == nil {
		return path
	}
	return filepath.Join(root, KindBin)
}

func ProjectRoot() (string, error) {
	if !IsDevMode() {
		return ensureExtractedAssets()
	}
	return findProjectRoot()
}

func findProjectRoot() (string, error) {
	dir, err := os.Getwd()
	if err != nil {
		return "", err
	}
	for {
		if _, err := os.Stat(filepath.Join(dir, "Makefile")); err == nil {
			if _, err := os.Stat(filepath.Join(dir, "simulator")); err == nil {
				return dir, nil
			}
		}
		parent := filepath.Dir(dir)
		if parent == dir {
			return "", fmt.Errorf("could not find picoshift project root (looking for Makefile + simulator/)")
		}
		dir = parent
	}
}

func ensureExtractedAssets() (string, error) {
	home, err := os.UserHomeDir()
	if err != nil {
		return "", fmt.Errorf("cannot determine home directory: %w", err)
	}
	cacheDir := filepath.Join(home, ".cache", "picoshift", versionTag())

	marker := filepath.Join(cacheDir, ".extracted")
	if _, err := os.Stat(marker); err == nil {
		return cacheDir, nil
	}

	fmt.Printf("Extracting embedded assets to %s...\n", cacheDir)
	if err := extractEmbeddedAssets(cacheDir); err != nil {
		return "", fmt.Errorf("failed to extract embedded assets: %w", err)
	}

	_ = os.WriteFile(marker, []byte(Version), 0644)
	return cacheDir, nil
}

func extractEmbeddedAssets(destDir string) error {
	if EmbeddedAssets == nil {
		return fmt.Errorf("no embedded assets available (built without embed_assets tag?)")
	}
	return fs.WalkDir(EmbeddedAssets, ".", func(path string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if path == "." {
			return nil
		}
		dest := filepath.Join(destDir, path)
		if d.IsDir() {
			return os.MkdirAll(dest, 0755)
		}
		data, err := fs.ReadFile(EmbeddedAssets, path)
		if err != nil {
			return err
		}
		if err := os.MkdirAll(filepath.Dir(dest), 0755); err != nil {
			return err
		}
		perm := os.FileMode(0644)
		if strings.HasSuffix(path, ".sh") || strings.HasSuffix(path, ".py") {
			perm = 0755
		}
		return os.WriteFile(dest, data, perm)
	})
}

func Sudo() string {
	if v := os.Getenv("SUDO"); v != "" {
		if v == "false" || v == "0" || v == "" {
			return ""
		}
		return v
	}
	return "sudo"
}
