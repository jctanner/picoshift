package cmd

import (
	"testing"

	"github.com/jctanner/picoshift/internal"
)

func TestModeValidation(t *testing.T) {
	tests := []struct {
		mode    string
		wantErr bool
	}{
		{"kind", false},
		{"namespace", false},
		{"docker", true},
		{"", true},
	}
	for _, tt := range tests {
		t.Run(tt.mode, func(t *testing.T) {
			valid := tt.mode == internal.ModeKind || tt.mode == internal.ModeNamespace
			if valid == tt.wantErr {
				t.Errorf("mode %q: valid=%v, wantErr=%v", tt.mode, valid, tt.wantErr)
			}
		})
	}
}

func TestKubectlWithCtx(t *testing.T) {
	args := kubectlWithCtx("kind-ocp-sim", "-n", "ocp-sim", "get", "pods")
	if len(args) != 6 || args[0] != "--context" || args[1] != "kind-ocp-sim" {
		t.Errorf("kind mode: got %v", args)
	}

	args = kubectlWithCtx("", "-n", "ocp-sim", "get", "pods")
	if len(args) != 4 || args[0] != "-n" {
		t.Errorf("namespace mode: got %v", args)
	}
}
