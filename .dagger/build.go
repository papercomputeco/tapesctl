package main

import (
	"context"
	"fmt"

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

// How a binary renders an identity. These mirror
// crates/tapesctl/src/build_info.rs, which is what allows a pipeline to state
// what it expects the artifact it just built to answer with, and so to notice
// when the answer is something else.
//
// The duplication is the point rather than a cost of it: a check that derived
// its expectation from the same code it is checking would agree with itself no
// matter what the binary did. It does mean a change to what a release binary
// prints has to be made here too — which fails loudly, before publishing, with
// a diff naming the change. Changing what a release identifies itself as is
// not something to slip out unremarked.
const (
	// What a build with no release tag calls itself.
	devVersion = "0.0.0-dev"
	// What is printed for a field nothing supplied.
	unknownField = "unknown"
	// How much of the commit rides along in the version string.
	shortSHALen = 7
)

// versionOutput is the whole of what `tapesctl --version` prints for a binary
// stamped with this identity — the version line clap composes from the binary
// name, then the commit and the build date.
func (id stampedIdentity) versionOutput() string {
	return fmt.Sprintf(
		"tapesctl %s\nSha: %s\nBuilt at: %s\n",
		id.reportedVersion(), orUnknown(id.commit), orUnknown(id.date),
	)
}

// reportedVersion is the version an artifact carrying this identity reports: a
// name, plus the commit that produced it when there is one.
func (id stampedIdentity) reportedVersion() string {
	name := id.version
	if name == "" {
		name = devVersion
	}

	if short := shortSHA(id.commit); short != "" {
		return name + "+" + short
	}

	return name
}

// shortSHA takes the leading characters of a commit by character rather than by
// byte, for the reason the crate does: a stamped value is whatever the pipeline
// passed, and this has to agree with the crate on every input, not just the
// well-formed ones.
func shortSHA(sha string) string {
	runes := []rune(sha)
	if len(runes) > shortSHALen {
		return string(runes[:shortSHALen])
	}

	return sha
}

func orUnknown(value string) string {
	if value == "" {
		return unknownField
	}

	return value
}

// assertionPlatform is the architecture the identity assertion runs a binary
// on. Pinned rather than left native so the check does not depend on the
// machine driving the pipeline: a release cut from an arm64 laptop asks the
// same binary the same question a CI runner does, at the cost of emulating it.
const assertionPlatform dagger.Platform = "linux/amd64"

// AssertStampedIdentity runs a built binary and fails unless it reports the
// identity this pipeline stamped into it.
//
// This is a pipeline step rather than a workflow step because what it protects
// is publication. The release and nightly functions sync artifacts to the
// public download prefix that `install.sh` and the documented URLs read from,
// and a check that runs after that has already let a mis-stamped binary out —
// blocking the GitHub release afterwards leaves the wrong binary live on the
// download host, which is where nearly everyone gets one. Run from in here it
// is ordered before publication by construction, whatever order a workflow
// happens to put its steps in.
//
// The linux/amd64 artifact is the one asked. All four come out of one build
// container holding one set of variables, so an injection either reached that
// container or reached none of them, and a static musl binary needs nothing
// but a kernel to answer.
func (t *Tapesctl) AssertStampedIdentity(
	ctx context.Context,

	// Artifact tree to check, laid out as <os>/<arch>/tapesctl.
	artifacts *dagger.Directory,

	// Release tag or channel name the artifacts were stamped with.
	//
	// +optional
	// +default=""
	version string,

	// Full commit the artifacts were stamped with.
	//
	// +optional
	// +default=""
	commit string,

	// RFC 3339 build timestamp the artifacts were stamped with.
	//
	// +optional
	// +default=""
	date string,
) error {
	identity := stampedIdentity{version: version, commit: commit, date: date}

	return t.assertStampedIdentity(ctx, artifacts, identity)
}

func (t *Tapesctl) assertStampedIdentity(
	ctx context.Context,
	artifacts *dagger.Directory,
	id stampedIdentity,
) error {
	const binary = "/artifacts/linux/amd64/tapesctl"

	// Both files are printed before they are compared: a diff alone tells you
	// the check failed, and the question anyone asks next is which side is
	// wrong.
	_, err := dag.Container(dagger.ContainerOpts{Platform: assertionPlatform}).
		From("alpine:latest").
		WithDirectory("/artifacts", artifacts).
		WithNewFile("/expected", id.versionOutput()).
		WithExec([]string{"sh", "-c", `
			set -eu
			chmod +x ` + binary + `
			` + binary + ` --version > /reported
			echo "stamped by this pipeline:"; cat /expected
			echo "reported by the binary:"; cat /reported
			diff -u /expected /reported
		`}).
		Sync(ctx)
	if err != nil {
		return fmt.Errorf("built binaries do not report the identity they were stamped with: %w", err)
	}

	return nil
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
	return t.build(stampedIdentity{version: version, commit: commit, date: date})
}

func (t *Tapesctl) build(identity stampedIdentity) *dagger.Directory {
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
//
// It does not check what the binaries report about themselves: that belongs to
// whatever is about to publish them, which is the only thing that both knows
// the identity it asked for and can still withhold the result. See
// [Tapesctl.AssertStampedIdentity].
func (t *Tapesctl) BuildRelease(
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
	return t.buildRelease(stampedIdentity{version: version, commit: commit, date: date})
}

func (t *Tapesctl) buildRelease(identity stampedIdentity) *dagger.Directory {
	return t.checksum(t.build(identity))
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
