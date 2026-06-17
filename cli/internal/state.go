package internal

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
)

type PicoshiftState struct {
	Mode     string `json:"mode"`
	Name     string `json:"name"`
	AuthMode string `json:"authMode,omitempty"`
}

func StatePath(name string) string {
	home, err := os.UserHomeDir()
	if err != nil {
		return ""
	}
	return filepath.Join(home, ".local", "state", "picoshift", name, "state.json")
}

func SaveState(s *PicoshiftState, path string) error {
	if err := os.MkdirAll(filepath.Dir(path), 0700); err != nil {
		return err
	}
	data, err := json.MarshalIndent(s, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(path, data, 0600)
}

func LoadState(path string) (*PicoshiftState, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return &PicoshiftState{Mode: ModeKind}, nil
	}
	var s PicoshiftState
	if err := json.Unmarshal(data, &s); err != nil {
		return &PicoshiftState{Mode: ModeKind}, nil
	}
	return &s, nil
}

func ValidateName(name string) error {
	if name == "" {
		return fmt.Errorf("cluster name cannot be empty")
	}
	if len(name) > 63 {
		return fmt.Errorf("cluster name %q exceeds 63 characters", name)
	}
	for _, c := range name {
		if !((c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') || c == '-') {
			return fmt.Errorf("cluster name %q contains invalid character %q (must be lowercase alphanumeric or hyphen)", name, string(c))
		}
	}
	if name[0] == '-' || name[len(name)-1] == '-' {
		return fmt.Errorf("cluster name %q must not start or end with a hyphen", name)
	}
	return nil
}

func RemoveState(path string) error {
	if path == "" {
		return nil
	}
	dir := filepath.Dir(path)
	if dir == "." || dir == "/" {
		return nil
	}
	return os.RemoveAll(dir)
}
