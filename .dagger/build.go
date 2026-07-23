package main

import (
	"context"

	"dagger/tapesctl/internal/dagger"
)

// buildTarget maps a Rust target triple to the `<os>/<arch>/` bucket layout the
// install script and release workflows expect. The layout is unchanged from the
// Go pipeline so `install.sh` (which fetches
// `.../tapesctl/<version>/<os>/<arch>/tapesctl`) keeps working.
type buildTarget struct {
	triple string
	os     string
	arch   string
}

var releaseTargets = []buildTarget{
	{"x86_64-unknown-linux-musl", "linux", "amd64"},
	{"aarch64-unknown-linux-musl", "linux", "arm64"},
	{"x86_64-apple-darwin", "darwin", "amd64"},
	{"aarch64-apple-darwin", "darwin", "arm64"},
}

// Build cross-compiles the tapesctl binary for all supported platforms using
// cargo-zigbuild. Linux targets are static musl builds (curl-and-run, like the
// old CGO_ENABLED=0 Go binaries); macOS targets link against zig's bundled
// libSystem stubs — no Apple SDK required.
func (t *Tapesctl) Build(_ context.Context) *dagger.Directory {
	base := t.rustContainer()
	outputs := dag.Directory()

	for _, target := range releaseTargets {
		dir := target.os + "/" + target.arch + "/"
		build := base.
			WithExec([]string{
				"cargo", "zigbuild", "--release", "--locked",
				"-p", "tapesctl", "--target", target.triple,
			}).
			WithExec([]string{"sh", "-c",
				"mkdir -p /out/" + dir +
					" && cp -a target/" + target.triple + "/release/tapesctl /out/" + dir + "tapesctl"})
		outputs = outputs.WithDirectory(dir, build.Directory("/out/"+dir))
	}

	return outputs
}

// BuildRelease compiles release binaries and adds SHA256 checksums.
func (t *Tapesctl) BuildRelease(ctx context.Context) *dagger.Directory {
	return t.checksum(t.Build(ctx))
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
