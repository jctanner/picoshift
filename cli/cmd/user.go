package cmd

import (
	"encoding/base64"
	"encoding/json"
	"fmt"
	"strings"

	"github.com/jctanner/picoshift/internal"
	"github.com/spf13/cobra"
)

func NewUserCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "user",
		Short: "Manage simulator users",
		Long: `Manage users for the picoshift simulator.

In legacy/oidc mode, users are stored in an htpasswd Secret in
openshift-config. In byoidc mode, users are managed via the
entra-mock admin API.`,
	}

	var name string
	cmd.PersistentFlags().StringVar(&name, "name", internal.DefaultClusterName, "Cluster name")

	cmd.AddCommand(
		newUserAddCmd(&name),
		newUserDeleteCmd(&name),
		newUserListCmd(&name),
	)
	return cmd
}

func resolveContext(clusterName string) string {
	statePath := internal.StatePath(clusterName)
	state, _ := internal.LoadState(statePath)
	if state.Mode == internal.ModeNamespace {
		return ""
	}
	return "kind-" + clusterName
}

func resolveAuthMode(clusterName string) string {
	statePath := internal.StatePath(clusterName)
	state, _ := internal.LoadState(statePath)
	if state.AuthMode != "" {
		return state.AuthMode
	}
	ctx := resolveContext(clusterName)
	if ctx != "" {
		return internal.GetAuthMode(ctx)
	}
	return internal.GetAuthModeFromResource("deployment", "")
}

func kubectlWithCtx(ctx string, args ...string) []string {
	if ctx != "" {
		return append([]string{"--context", ctx}, args...)
	}
	return args
}

// --- add ---

func newUserAddCmd(clusterName *string) *cobra.Command {
	var (
		username     string
		password     string
		email        string
		clusterAdmin bool
	)

	cmd := &cobra.Command{
		Use:   "add",
		Short: "Add or update a user",
		RunE: func(cmd *cobra.Command, args []string) error {
			if username == "" || password == "" {
				return fmt.Errorf("--username and --password are required")
			}
			ctx := resolveContext(*clusterName)
			mode := resolveAuthMode(*clusterName)

			if mode == "byoidc" {
				if err := entraMockAddUser(ctx, username, password, email); err != nil {
					return err
				}
			} else {
				if err := htpasswdAddUser(ctx, username, password); err != nil {
					return err
				}
			}

			if clusterAdmin {
				crbName := fmt.Sprintf("picoshift-admin-%s", username)
				_ = internal.RunQuiet("kubectl", kubectlWithCtx(ctx,
					"delete", "clusterrolebinding", crbName, "--ignore-not-found")...)
				if err := internal.Run("kubectl", kubectlWithCtx(ctx,
					"create", "clusterrolebinding", crbName,
					"--clusterrole=cluster-admin",
					fmt.Sprintf("--user=%s", username),
				)...); err != nil {
					return fmt.Errorf("failed to create ClusterRoleBinding: %w", err)
				}
				fmt.Printf("  cluster-admin granted to %s\n", username)
			}

			return nil
		},
	}

	cmd.Flags().StringVar(&username, "username", "", "Username (required)")
	cmd.Flags().StringVar(&password, "password", "", "Password (required)")
	cmd.Flags().StringVar(&email, "email", "", "Email address (byoidc only)")
	cmd.Flags().BoolVar(&clusterAdmin, "cluster-admin", false, "Grant cluster-admin role")
	return cmd
}

// --- delete ---

func newUserDeleteCmd(clusterName *string) *cobra.Command {
	var username string

	cmd := &cobra.Command{
		Use:   "delete",
		Short: "Delete a user",
		RunE: func(cmd *cobra.Command, args []string) error {
			if username == "" {
				return fmt.Errorf("--username is required")
			}
			ctx := resolveContext(*clusterName)
			mode := resolveAuthMode(*clusterName)

			if mode == "byoidc" {
				if err := entraMockDeleteUser(ctx, username); err != nil {
					return err
				}
			} else {
				if err := htpasswdDeleteUser(ctx, username); err != nil {
					return err
				}
			}

			// Clean up K8s objects
			_ = internal.RunQuiet("kubectl", kubectlWithCtx(ctx,
				"delete", "user.user.openshift.io", username, "--ignore-not-found")...)
			_ = internal.RunQuiet("kubectl", kubectlWithCtx(ctx,
				"delete", "identity.user.openshift.io", fmt.Sprintf("ocp-sim.%s", username), "--ignore-not-found")...)
			_ = internal.RunQuiet("kubectl", kubectlWithCtx(ctx,
				"delete", "clusterrolebinding", fmt.Sprintf("picoshift-admin-%s", username), "--ignore-not-found")...)

			fmt.Printf("User %q deleted\n", username)
			return nil
		},
	}

	cmd.Flags().StringVar(&username, "username", "", "Username to delete (required)")
	return cmd
}

// --- list ---

func newUserListCmd(clusterName *string) *cobra.Command {
	cmd := &cobra.Command{
		Use:   "list",
		Short: "List users",
		RunE: func(cmd *cobra.Command, args []string) error {
			ctx := resolveContext(*clusterName)
			mode := resolveAuthMode(*clusterName)

			if mode == "byoidc" {
				return entraMockListUsers(ctx)
			}
			return htpasswdListUsers(ctx)
		},
	}
	return cmd
}

// ---------------------------------------------------------------------------
// htpasswd helpers (legacy/oidc)
// ---------------------------------------------------------------------------

func readHtpasswd(ctx string) (string, error) {
	out, err := internal.RunOutputQuiet("kubectl", kubectlWithCtx(ctx,
		"-n", internal.HtpasswdNamespace,
		"get", "secret", internal.HtpasswdSecret,
		"-o", "jsonpath={.data.htpasswd}")...)
	if err != nil {
		return "", nil // secret doesn't exist yet
	}
	decoded, err := base64.StdEncoding.DecodeString(strings.TrimSpace(out))
	if err != nil {
		return "", fmt.Errorf("failed to decode htpasswd: %w", err)
	}
	return string(decoded), nil
}

func applyHtpasswdSecret(ctx, htpasswd string) error {
	secret := fmt.Sprintf(`apiVersion: v1
kind: Secret
metadata:
  name: %s
  namespace: %s
type: Opaque
stringData:
  htpasswd: |
`, internal.HtpasswdSecret, internal.HtpasswdNamespace)
	for _, line := range strings.Split(strings.TrimRight(htpasswd, "\n"), "\n") {
		secret += "    " + line + "\n"
	}
	ctxFlag := ""
	if ctx != "" {
		ctxFlag = "--context " + ctx + " "
	}
	return internal.Run("bash", "-c",
		fmt.Sprintf("cat <<'HTPASSWD_EOF' | kubectl %sapply -f -\n%sHTPASSWD_EOF", ctxFlag, secret))
}

func htpasswdAddUser(ctx, username, password string) error {
	// Ensure namespace exists
	_ = internal.RunQuiet("kubectl", kubectlWithCtx(ctx,
		"create", "namespace", internal.HtpasswdNamespace)...)

	current, err := readHtpasswd(ctx)
	if err != nil {
		return err
	}

	lines := strings.Split(strings.TrimSpace(current), "\n")
	found := false
	var updated []string
	for _, line := range lines {
		if line == "" {
			continue
		}
		parts := strings.SplitN(line, ":", 2)
		if len(parts) == 2 && parts[0] == username {
			updated = append(updated, fmt.Sprintf("%s:%s", username, password))
			found = true
		} else {
			updated = append(updated, line)
		}
	}
	if !found {
		updated = append(updated, fmt.Sprintf("%s:%s", username, password))
	}

	htpasswd := strings.Join(updated, "\n") + "\n"

	if err := applyHtpasswdSecret(ctx, htpasswd); err != nil {
		return fmt.Errorf("failed to update htpasswd secret: %w", err)
	}

	if found {
		fmt.Printf("User %q updated\n", username)
	} else {
		fmt.Printf("User %q added\n", username)
	}
	return nil
}

func htpasswdDeleteUser(ctx, username string) error {
	current, err := readHtpasswd(ctx)
	if err != nil {
		return err
	}
	if current == "" {
		return fmt.Errorf("htpasswd secret not found")
	}

	lines := strings.Split(strings.TrimSpace(current), "\n")
	var updated []string
	found := false
	for _, line := range lines {
		parts := strings.SplitN(line, ":", 2)
		if len(parts) == 2 && parts[0] == username {
			found = true
			continue
		}
		if line != "" {
			updated = append(updated, line)
		}
	}
	if !found {
		return fmt.Errorf("user %q not found in htpasswd", username)
	}

	htpasswd := strings.Join(updated, "\n") + "\n"

	return applyHtpasswdSecret(ctx, htpasswd)
}

func htpasswdListUsers(ctx string) error {
	current, err := readHtpasswd(ctx)
	if err != nil {
		return err
	}
	if current == "" {
		fmt.Println("No htpasswd secret found")
		return nil
	}

	fmt.Printf("%-20s %s\n", "USERNAME", "CLUSTER-ADMIN")
	for _, line := range strings.Split(strings.TrimSpace(current), "\n") {
		parts := strings.SplitN(line, ":", 2)
		if len(parts) != 2 || parts[0] == "" {
			continue
		}
		username := parts[0]
		admin := "no"
		crbName := fmt.Sprintf("picoshift-admin-%s", username)
		if internal.RunQuiet("kubectl", kubectlWithCtx(ctx,
			"get", "clusterrolebinding", crbName)...) == nil {
			admin = "yes"
		}
		// Also check the ocp-sim-admin binding for the default admin user
		if username == "admin" && admin == "no" {
			if internal.RunQuiet("kubectl", kubectlWithCtx(ctx,
				"get", "clusterrolebinding", "ocp-sim-admin")...) == nil {
				admin = "yes"
			}
		}
		fmt.Printf("%-20s %s\n", username, admin)
	}
	return nil
}

// ---------------------------------------------------------------------------
// entra-mock helpers (byoidc)
// ---------------------------------------------------------------------------

func entraMockExec(ctx string, curlArgs ...string) (string, error) {
	args := kubectlWithCtx(ctx,
		"-n", internal.EntraMockNamespace,
		"exec", "deploy/entra-mock", "--",
		"curl", "-s", "-u", ":"+internal.EntraMockAdminPass,
	)
	args = append(args, curlArgs...)
	return internal.RunOutputQuiet("kubectl", args...)
}

func entraMockAddUser(ctx, username, password, email string) error {
	if email == "" {
		email = fmt.Sprintf("%s@ocp-sim.test", username)
	}

	payload := map[string]interface{}{
		"tenant_id":    internal.EntraMockTenantID,
		"upn":          username,
		"email":        email,
		"display_name": username,
		"password":     password,
		"groups": []map[string]string{
			{"id": "g0000000-0000-0000-0000-authenticated0", "name": "system:authenticated"},
		},
	}
	data, _ := json.Marshal(payload)

	out, err := entraMockExec(ctx,
		"-X", "POST",
		"http://localhost:8080/admin/api/users",
		"-H", "Content-Type: application/json",
		"-d", string(data),
	)
	if err != nil {
		return fmt.Errorf("failed to add user to entra-mock: %w", err)
	}

	if strings.Contains(out, "error") {
		return fmt.Errorf("entra-mock error: %s", out)
	}

	fmt.Printf("User %q added to entra-mock\n", username)
	return nil
}

func entraMockDeleteUser(ctx, username string) error {
	// List users to find the ID
	out, err := entraMockExec(ctx,
		"http://localhost:8080/admin/api/users",
	)
	if err != nil {
		return fmt.Errorf("failed to list entra-mock users: %w", err)
	}

	var users []struct {
		ID  string `json:"id"`
		UPN string `json:"upn"`
	}
	if err := json.Unmarshal([]byte(out), &users); err != nil {
		return fmt.Errorf("failed to parse entra-mock users: %w", err)
	}

	var userID string
	for _, u := range users {
		if u.UPN == username {
			userID = u.ID
			break
		}
	}
	if userID == "" {
		return fmt.Errorf("user %q not found in entra-mock", username)
	}

	_, err = entraMockExec(ctx,
		"-X", "DELETE",
		fmt.Sprintf("http://localhost:8080/admin/api/users/%s", userID),
	)
	if err != nil {
		return fmt.Errorf("failed to delete user from entra-mock: %w", err)
	}

	fmt.Printf("User %q deleted from entra-mock\n", username)
	return nil
}

func entraMockListUsers(ctx string) error {
	out, err := entraMockExec(ctx,
		"http://localhost:8080/admin/api/users",
	)
	if err != nil {
		return fmt.Errorf("failed to list entra-mock users: %w", err)
	}

	var users []struct {
		UPN   string `json:"upn"`
		Email string `json:"email"`
	}
	if err := json.Unmarshal([]byte(out), &users); err != nil {
		return fmt.Errorf("failed to parse entra-mock users: %w", err)
	}

	fmt.Printf("%-20s %-30s %s\n", "USERNAME", "EMAIL", "CLUSTER-ADMIN")
	for _, u := range users {
		admin := "no"
		crbName := fmt.Sprintf("picoshift-admin-%s", u.UPN)
		if internal.RunQuiet("kubectl", kubectlWithCtx(ctx,
			"get", "clusterrolebinding", crbName)...) == nil {
			admin = "yes"
		}
		if u.UPN == "admin" && admin == "no" {
			if internal.RunQuiet("kubectl", kubectlWithCtx(ctx,
				"get", "clusterrolebinding", "ocp-sim-admin")...) == nil {
				admin = "yes"
			}
		}
		fmt.Printf("%-20s %-30s %s\n", u.UPN, u.Email, admin)
	}
	return nil
}
