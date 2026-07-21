package main

import (
	"context"
	"errors"
	"fmt"

	"dagger/tapesctl/internal/dagger"
)

// CheckGoModTidy fails when go mod tidy changes go.mod or go.sum.
//
// +check
func (t *Tapesctl) CheckGoModTidy(ctx context.Context) (string, error) {
	out, err := t.goContainer().
		WithExec([]string{"cp", "go.mod", "go.mod.HEAD"}).
		WithExec([]string{"cp", "go.sum", "go.sum.HEAD"}).
		WithExec([]string{"go", "mod", "tidy"}).
		WithExec([]string{
			"sh", "-c",
			"diff -u go.mod.HEAD go.mod && diff -u go.sum.HEAD go.sum",
		}).
		Stdout(ctx)

	var execErr *dagger.ExecError
	if errors.As(err, &execErr) {
		return "", fmt.Errorf(
			"go.mod or go.sum are not tidy: run 'go mod tidy' and commit the changes\n\n%s",
			execErr.Stdout,
		)
	}
	if err != nil {
		return "", fmt.Errorf("unexpected error: %w", err)
	}

	return fmt.Sprintf("go.mod and go.sum are tidy: %s", out), nil
}
