package internal

import (
	"fmt"
	"os"
	"os/exec"
	"strings"
)

var DryRun bool

func Run(name string, args ...string) error {
	if DryRun {
		fmt.Printf("  [dry-run] %s %s\n", name, strings.Join(args, " "))
		return nil
	}
	cmd := exec.Command(name, args...)
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr
	cmd.Stdin = os.Stdin
	if err := cmd.Run(); err != nil {
		return fmt.Errorf("%s %s: %w", name, strings.Join(args, " "), err)
	}
	return nil
}

func RunStep(step, total int, desc, name string, args ...string) error {
	fmt.Printf("[%d/%d] %s\n", step, total, desc)
	return Run(name, args...)
}

func RunOutput(name string, args ...string) (string, error) {
	if DryRun {
		fmt.Printf("  [dry-run] %s %s\n", name, strings.Join(args, " "))
		return "", nil
	}
	cmd := exec.Command(name, args...)
	cmd.Stderr = os.Stderr
	out, err := cmd.Output()
	if err != nil {
		return "", fmt.Errorf("%s %s: %w", name, strings.Join(args, " "), err)
	}
	return strings.TrimSpace(string(out)), nil
}

func RunSudo(args ...string) error {
	sudo := Sudo()
	if sudo == "" {
		return Run(args[0], args[1:]...)
	}
	return Run(sudo, args...)
}

func RunSudoStep(step, total int, desc string, args ...string) error {
	fmt.Printf("[%d/%d] %s\n", step, total, desc)
	return RunSudo(args...)
}

func RunSudoOutput(args ...string) (string, error) {
	sudo := Sudo()
	if sudo == "" {
		return RunOutput(args[0], args[1:]...)
	}
	return RunOutput(sudo, args...)
}

func RunQuiet(name string, args ...string) error {
	if DryRun {
		return nil
	}
	cmd := exec.Command(name, args...)
	if err := cmd.Run(); err != nil {
		return fmt.Errorf("%s %s: %w", name, strings.Join(args, " "), err)
	}
	return nil
}

func RunOutputQuiet(name string, args ...string) (string, error) {
	if DryRun {
		return "", nil
	}
	cmd := exec.Command(name, args...)
	out, err := cmd.Output()
	if err != nil {
		return "", fmt.Errorf("%s %s: %w", name, strings.Join(args, " "), err)
	}
	return strings.TrimSpace(string(out)), nil
}

func CheckDep(name string) error {
	_, err := exec.LookPath(name)
	if err != nil {
		return fmt.Errorf("required dependency %q not found in PATH", name)
	}
	return nil
}

func CheckFile(path string) error {
	if _, err := os.Stat(path); err != nil {
		return fmt.Errorf("required file not found: %s", path)
	}
	return nil
}
