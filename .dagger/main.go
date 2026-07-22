// tapesctl CI/CD
//
// Package main provides reproducible lint, test, and cross-platform release
// builds locally and in GitHub Actions.
//
// tapesctl is a pure-Rust CLI (no Apple frameworks, no C deps), so the
// Linux→macOS cross-compile uses `cargo-zigbuild` — zig bundles the macOS
// libSystem stubs, so no Apple SDK and no osxcross build are needed (unlike
// `platform/paper`, whose AppKit menu code forces the heavier osxcross path).
//
// To wire up after first checkout:
//
//	cd tapesctl
//	dagger develop            # regenerates go.mod + dagger codegen
//	dagger call lint          # cargo fmt --check + clippy
//	dagger call test          # cargo test --workspace
//	dagger call build-release export --path ./build
package main

import (
	"context"

	"dagger/tapesctl/internal/dagger"
)

// Pinned cross-compile toolchain. Bumped intentionally.
const (
	rustImage            = "rust:1.85-bookworm"
	zigVersion           = "0.13.0"
	cargoZigbuildVersion = "0.19.8"
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
	// +ignore=[".git", ".direnv", "target", "build", "tmp"]
	source *dagger.Directory,
) *Tapesctl {
	return &Tapesctl{Source: source}
}

// rustContainer returns the shared Rust container used by lint, test, and
// build. It installs zig + cargo-zigbuild and every release target's std so a
// single base can lint, test, and cross-compile all four platforms. The cargo
// registry and the workspace `target/` are cached across `dagger call`
// invocations.
func (t *Tapesctl) rustContainer() *dagger.Container {
	cargoRegistry := dag.CacheVolume("tapesctl-cargo-registry")
	cargoTarget := dag.CacheVolume("tapesctl-cargo-target")

	// zig ships per-arch; match the Dagger worker's arch at runtime so this
	// works on both x86_64 CI runners and aarch64 dev laptops.
	installZig := `set -eux
arch="$(uname -m)"
url="https://ziglang.org/download/` + zigVersion + `/zig-linux-${arch}-` + zigVersion + `.tar.xz"
curl -fsSL "$url" -o /tmp/zig.tar.xz
mkdir -p /opt/zig
tar -xJf /tmp/zig.tar.xz -C /opt/zig --strip-components=1
rm /tmp/zig.tar.xz`

	return dag.Container().
		From(rustImage).
		WithMountedCache("/usr/local/cargo/registry", cargoRegistry).
		WithExec([]string{"apt-get", "update"}).
		WithExec([]string{"apt-get", "install", "-y", "--no-install-recommends",
			"xz-utils", "ca-certificates", "curl"}).
		WithExec([]string{"sh", "-c", installZig}).
		WithEnvVariable("PATH", "/opt/zig:$PATH",
			dagger.ContainerWithEnvVariableOpts{Expand: true}).
		WithExec([]string{"cargo", "install", "cargo-zigbuild",
			"--version", cargoZigbuildVersion, "--locked"}).
		WithWorkdir("/src").
		WithDirectory("/src", t.Source).
		WithMountedCache("/src/target", cargoTarget).
		// Materialize the toolchain declared in rust-toolchain.toml (channel =
		// stable) BEFORE adding targets — otherwise `rustup target add` targets
		// the image's default 1.85 toolchain while cargo builds under stable,
		// and the cross-target std is missing ("can't find crate for std").
		WithExec([]string{"rustup", "show"}).
		WithExec([]string{"rustup", "target", "add",
			"x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl",
			"x86_64-apple-darwin", "aarch64-apple-darwin"}).
		// Incremental artifacts are pure waste in CI and bloat the target
		// cache; nothing resumes a compile session here.
		WithEnvVariable("CARGO_INCREMENTAL", "0")
}

// Test runs the workspace unit tests. `--locked` fails if Cargo.lock is stale,
// which subsumes the go.mod-tidy check the Go pipeline had.
//
// +check
func (t *Tapesctl) Test(ctx context.Context) (string, error) {
	return t.rustContainer().
		WithExec([]string{"cargo", "test", "--workspace", "--locked"}).
		Stdout(ctx)
}
