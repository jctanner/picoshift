package internal

import (
	"fmt"
	"os"
	"path/filepath"
)

const (
	DefaultClusterName = "ocp-sim"
	DefaultK8sVersion  = "v1.33.1"
	DefaultAuthMode    = "legacy"

	KindForkDir = "deps/kind"
	KindBin     = "deps/kind/bin/kind"
	BaseImage   = "kindest/base:ocp-shim"
	NodeImage   = "localhost/kindest/node:ocp-shim"
	SimImage       = "localhost/ocp-sim:latest"
	EntraMockImage = "localhost/entra-mock:latest"

	KindConfig  = "deploy/kind/cluster.yaml"
	SimManifest = "deploy/simulator.yaml"
	UsersFile   = "deploy/users.yaml"

	SimNamespace       = "ocp-sim"
	OperatorNamespace  = "opendatahub-operator-system"
	IstioNamespace     = "istio-system"
	EntraMockNamespace = "entra-mock"

	HtpasswdSecret    = "htpass-secret"
	HtpasswdNamespace = "openshift-config"
	EntraMockAdminPass = "changeme1234"
	EntraMockTenantID  = "a1b2c3d4-e5f6-7890-abcd-ef1234567890"

	OLMDefaultVersion = "v0.42.0"
	OLMNamespace      = "olm"
)

func ProjectRoot() (string, error) {
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

func Sudo() string {
	if v := os.Getenv("SUDO"); v != "" {
		if v == "false" || v == "0" || v == "" {
			return ""
		}
		return v
	}
	return "sudo"
}
