// tapesctl CI/CD
//
// Package main provides reproducible builds and tests locally and in GitHub Actions.
package main

import (
	"context"

	"dagger/tapesctl/internal/dagger"
)

// Tapesctl is the main module for the tapesctl CI/CD pipeline.
type Tapesctl struct {
	// Project source directory.
	//
	// +private
	Source *dagger.Directory
}

// New creates a tapesctl CI/CD module instance.
func New(
	// Project source directory.
	//
	// +defaultPath="/"
	// +ignore=[".git", ".direnv", "build", "tmp"]
	source *dagger.Directory,
) *Tapesctl {
	return &Tapesctl{Source: source}
}

// goContainer returns the shared Go container used by tests, builds, and linting.
func (t *Tapesctl) goContainer() *dagger.Container {
	return dag.Container().
		From("golang:1.25-bookworm").
		WithEnvVariable("CGO_ENABLED", "0").
		WithEnvVariable("PATH", "/go/bin:$PATH", dagger.ContainerWithEnvVariableOpts{Expand: true}).
		WithMountedCache("/go/pkg/mod", dag.CacheVolume("go-mod")).
		WithMountedCache("/root/.cache/go-build", dag.CacheVolume("go-build")).
		WithWorkdir("/src").
		WithDirectory("/src", t.Source)
}

// Test runs the tapesctl unit tests.
//
// +check
func (t *Tapesctl) Test(ctx context.Context) (string, error) {
	return t.goContainer().
		WithExec([]string{"go", "test", "-v", "./..."}).
		Stdout(ctx)
}
