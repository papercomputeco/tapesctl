package main

import (
	"context"
	"fmt"

	"dagger/tapesctl/internal/dagger"
)

type buildTarget struct {
	goos   string
	goarch string
}

// Build compiles tapesctl for all supported platforms.
func (t *Tapesctl) Build(
	_ context.Context,

	// Linker flags for go build.
	// +optional
	// +default="-s -w"
	ldflags string,
) *dagger.Directory {
	targets := []buildTarget{
		{"linux", "amd64"},
		{"linux", "arm64"},
		{"darwin", "amd64"},
		{"darwin", "arm64"},
	}

	golang := t.goContainer()
	outputs := dag.Directory()

	for _, target := range targets {
		path := fmt.Sprintf("%s/%s/", target.goos, target.goarch)

		build := golang.
			WithEnvVariable("GOOS", target.goos).
			WithEnvVariable("GOARCH", target.goarch).
			WithExec([]string{
				"go", "build", "-ldflags", ldflags,
				"-o", path + "tapesctl", "./cli/tapesctl",
			})

		outputs = outputs.WithDirectory(path, build.Directory(path))
	}

	return outputs
}

// BuildRelease compiles release binaries and adds SHA256 checksums.
func (t *Tapesctl) BuildRelease(ctx context.Context) *dagger.Directory {
	return t.checksum(t.Build(ctx, "-s -w"))
}

// checksum generates a SHA256 sidecar for every artifact.
func (t *Tapesctl) checksum(dir *dagger.Directory) *dagger.Directory {
	return dag.Container().
		From("alpine:latest").
		WithDirectory("/artifacts", dir).
		WithWorkdir("/artifacts").
		WithExec([]string{"sh", "-c", `
			find . -type f ! -name "*.sha256" | while read file; do
				dir="$(dirname "$file")"
				name="$(basename "$file")"
				(cd "$dir" && sha256sum "$name" > "$name.sha256")
			done
		`}).
		Directory("/artifacts")
}
