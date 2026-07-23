package main

import (
	"context"
)

// Lint runs `cargo fmt --all -- --check` and `cargo clippy --workspace
// --all-targets -- -D warnings`. Both are the same gates `make lint` runs
// locally, so a green Dagger lint matches a green local lint.
//
// +check
func (t *Tapesctl) Lint(ctx context.Context) (string, error) {
	return t.rustContainer().
		WithExec([]string{"cargo", "fmt", "--all", "--", "--check"}).
		WithExec([]string{"cargo", "clippy", "--workspace", "--all-targets",
			"--locked", "--", "-D", "warnings"}).
		Stdout(ctx)
}
