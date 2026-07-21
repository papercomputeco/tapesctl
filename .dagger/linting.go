package main

import (
	"context"
	"fmt"

	"dagger/tapesctl/internal/dagger"
)

const golangciLintVersion = "v2.8.0"

// lintOpts returns the common options used by lint checks and fixes.
func (t *Tapesctl) lintOpts() dagger.GolangcilintOpts {
	base := t.goContainer().
		WithExec([]string{
			"go", "install",
			fmt.Sprintf("github.com/golangci/golangci-lint/v2/cmd/golangci-lint@%s", golangciLintVersion),
		})

	return dagger.GolangcilintOpts{
		BaseCtr: base,
		Config:  t.Source.File(".golangci.yml"),
	}
}

// CheckLint runs golangci-lint without applying fixes.
//
// +check
func (t *Tapesctl) CheckLint(ctx context.Context) (string, error) {
	return dag.Golangcilint(t.Source, t.lintOpts()).Check(ctx)
}

// FixLint runs golangci-lint with automatic fixes and returns the modified source.
func (t *Tapesctl) FixLint(ctx context.Context) *dagger.Directory {
	return dag.Golangcilint(t.Source, t.lintOpts()).Lint()
}
