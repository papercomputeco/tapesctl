// tapesctl CI/CD
//
// Package main provides reproducible lint, test, and cross-platform release
// builds locally and in GitHub Actions.
//
// tapesctl is a pure-Rust CLI (no Apple frameworks, no C deps), so the
// Linux→macOS cross-compile uses `cargo-zigbuild` — zig bundles the macOS
// libSystem stubs, so no Apple SDK and no osxcross build are needed (unlike a
// workspace with Apple-framework code, which forces the heavier osxcross path).
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
	// works on both x86_64 CI runners and aarch64 dev laptops. The tarball is
	// checksum-verified against zig's published SHA256 — pinned per (version,
	// arch), so bump both together when bumping zigVersion (values from
	// https://ziglang.org/download/<version>/).
	installZig := `set -eux
arch="$(uname -m)"
case "$arch" in
  x86_64)  sha="d45312e61ebcc48032b77bc4cf7fd6915c11fa16e4aad116b66c9468211230ea" ;;
  aarch64) sha="041ac42323837eb5624068acd8b00cd5777dac4cf91179e8dad7a7e90dd0c556" ;;
  *) echo "unsupported arch: $arch" >&2; exit 1 ;;
esac
url="https://ziglang.org/download/` + zigVersion + `/zig-linux-${arch}-` + zigVersion + `.tar.xz"
curl -fsSL "$url" -o /tmp/zig.tar.xz
echo "${sha}  /tmp/zig.tar.xz" | sha256sum -c -
mkdir -p /opt/zig
tar -xJf /tmp/zig.tar.xz -C /opt/zig --strip-components=1
rm /tmp/zig.tar.xz`

	return dag.Container().
		From(rustImage).
		// Locked sharing serializes registry writes: Build cross-compiles four
		// targets off this one base, and Dagger runs them concurrently. With the
		// default Shared mode they race unpacking the same crates and one fails
		// with `.cargo-ok: File exists (os error 17)`. (The same fix the daemon
		// client's pipeline uses.)
		WithMountedCache("/usr/local/cargo/registry", cargoRegistry,
			dagger.ContainerWithMountedCacheOpts{
				Sharing: dagger.CacheSharingModeLocked,
			}).
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
