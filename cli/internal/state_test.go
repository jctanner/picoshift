package internal

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestSaveAndLoadState(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "state.json")

	s := &PicoshiftState{
		Mode:     ModeNamespace,
		Name:     "test-ns",
		AuthMode: "legacy",
	}
	if err := SaveState(s, path); err != nil {
		t.Fatalf("SaveState: %v", err)
	}

	info, err := os.Stat(path)
	if err != nil {
		t.Fatalf("Stat: %v", err)
	}
	if info.Mode().Perm() != 0600 {
		t.Errorf("file permissions = %o, want 0600", info.Mode().Perm())
	}

	loaded, err := LoadState(path)
	if err != nil {
		t.Fatalf("LoadState: %v", err)
	}
	if loaded.Mode != ModeNamespace {
		t.Errorf("Mode = %q, want %q", loaded.Mode, ModeNamespace)
	}
	if loaded.Name != "test-ns" {
		t.Errorf("Name = %q, want %q", loaded.Name, "test-ns")
	}
}

func TestLoadStateMissing(t *testing.T) {
	s, err := LoadState("/nonexistent/path/state.json")
	if err != nil {
		t.Fatalf("LoadState should not error on missing file: %v", err)
	}
	if s.Mode != ModeKind {
		t.Errorf("default Mode = %q, want %q", s.Mode, ModeKind)
	}
}

func TestStatePath(t *testing.T) {
	path := StatePath("my-cluster")
	if path == "" {
		t.Fatal("StatePath returned empty string")
	}
	if filepath.Base(path) != "state.json" {
		t.Errorf("StatePath base = %q, want state.json", filepath.Base(path))
	}
	if dir := filepath.Dir(path); !strings.Contains(dir, ".local/state/picoshift") {
		t.Errorf("StatePath dir = %q, want to contain .local/state/picoshift", dir)
	}
}
