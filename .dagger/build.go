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

// stampedIdentity is the build identity tapesctl reports as its version. The
// crate's build script reads these three variables; see
// crates/tapesctl/build.rs. Anything left empty is not injected at all, and the
// binary reports a development version for that field — which is the honest
// answer for a build no release pipeline cut.
type stampedIdentity struct {
	// Release tag or channel name, for example v1.0.0 or nightly.
	version string
	// Full commit the artifact is built from.
	commit string
	// RFC 3339 build timestamp.
	date string
}

// stamp injects the identity into a build container.
//
// The order is fixed rather than map iteration order, because each
// WithEnvVariable is a layer: a varying order would give the same build a
// different digest every call and defeat the cache.
func (id stampedIdentity) stamp(container *dagger.Container) *dagger.Container {
	for _, variable := range []struct{ name, value string }{
		{"TAPESCTL_RELEASE_TAG", id.version},
		{"TAPESCTL_BUILD_SHA", id.commit},
		{"TAPESCTL_BUILD_DATE", id.date},
	} {
		if variable.value == "" {
			continue
		}
		container = container.WithEnvVariable(variable.name, variable.value)
	}

	return container
}

// Build cross-compiles the tapesctl binary for all supported platforms using
// cargo-zigbuild. Linux targets are static musl builds (curl-and-run, like the
// old CGO_ENABLED=0 Go binaries); macOS targets link against zig's bundled
// libSystem stubs — no Apple SDK required.
//
// The identity arguments are what make a released binary able to name itself.
// They are optional because this function also serves plain CI builds, which
// have no release to name: the source arrives here without a .git directory
// (see the module's +ignore), so an unstamped build genuinely knows nothing
// about its own provenance and says so rather than guessing.
func (t *Tapesctl) Build(
	_ context.Context,

	// Release tag or channel name to stamp into the binaries, for example
	// v1.0.0 or nightly.
	//
	// +optional
	// +default=""
	version string,

	// Full commit the binaries are built from.
	//
	// +optional
	// +default=""
	commit string,

	// RFC 3339 timestamp of the build.
	//
	// +optional
	// +default=""
	date string,
) *dagger.Directory {
	identity := stampedIdentity{version: version, commit: commit, date: date}
	base := identity.stamp(t.rustContainer())
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
func (t *Tapesctl) BuildRelease(
	ctx context.Context,

	// Release tag or channel name to stamp into the binaries, for example
	// v1.0.0 or nightly.
	//
	// +optional
	// +default=""
	version string,

	// Full commit the binaries are built from.
	//
	// +optional
	// +default=""
	commit string,

	// RFC 3339 timestamp of the build.
	//
	// +optional
	// +default=""
	date string,
) *dagger.Directory {
	return t.checksum(t.Build(ctx, version, commit, date))
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
