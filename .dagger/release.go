package main

import (
	"context"
	"fmt"
	"path"
	"time"

	"dagger/tapesctl/internal/dagger"
)

const artifactPrefix = "tapesctl"

// nightlyVersion is the name a nightly artifact reports as its version. It is
// also the git tag the nightly workflow moves to the built commit, so the name
// resolves to something. On its own it identifies no particular build, which is
// why the commit is stamped alongside it and ends up in the version string.
const nightlyVersion = "nightly"

// buildTimestamp is the `Built at:` field of every artifact this file produces.
// Taken once per invocation so the four cross-compiled binaries of one release
// agree on when they were built.
func buildTimestamp() string {
	return time.Now().UTC().Format(time.RFC3339)
}

type uploadOpts struct {
	artifacts       *dagger.Directory
	prefix          string
	endpoint        *dagger.Secret
	bucket          *dagger.Secret
	accessKeyID     *dagger.Secret
	secretAccessKey *dagger.Secret
}

// upload syncs artifacts to an S3-compatible bucket.
func (t *Tapesctl) upload(ctx context.Context, opts *uploadOpts) error {
	bucketName, err := opts.bucket.Plaintext(ctx)
	if err != nil {
		return fmt.Errorf("failed to get bucket name: %w", err)
	}

	endpointURL, err := opts.endpoint.Plaintext(ctx)
	if err != nil {
		return fmt.Errorf("failed to get bucket endpoint: %w", err)
	}

	destination := fmt.Sprintf("s3://%s", path.Join(bucketName, opts.prefix))

	_, err = dag.Container().
		From("amazon/aws-cli:latest").
		WithSecretVariable("AWS_ACCESS_KEY_ID", opts.accessKeyID).
		WithSecretVariable("AWS_SECRET_ACCESS_KEY", opts.secretAccessKey).
		WithEnvVariable("AWS_DEFAULT_REGION", "auto").
		WithDirectory("/artifacts", opts.artifacts).
		WithWorkdir("/artifacts").
		WithExec([]string{
			"aws", "s3", "sync", ".", destination,
			"--endpoint-url", endpointURL,
		}).
		Sync(ctx)
	if err != nil {
		return fmt.Errorf("failed to upload artifacts: %w", err)
	}

	return nil
}

// ReleaseLatest builds and uploads versioned and latest release artifacts.
func (t *Tapesctl) ReleaseLatest(
	ctx context.Context,

	// Version string, for example v1.0.0. Names the upload prefix and is
	// stamped into the binaries, so a released tapesctl reports the release it
	// was downloaded from.
	//
	// Written without backquotes on purpose: the help renderer takes a
	// backquoted word as the flag's placeholder, so a quoted command name here
	// would be printed where the argument's type belongs.
	version string,

	// Full commit the release is built from.
	//
	// +optional
	// +default=""
	commit string,

	// Bucket endpoint URL.
	endpoint *dagger.Secret,

	// Bucket name.
	bucket *dagger.Secret,

	// Bucket access key ID.
	accessKeyID *dagger.Secret,

	// Bucket secret access key.
	secretAccessKey *dagger.Secret,
) (*dagger.Directory, error) {
	identity := stampedIdentity{version: version, commit: commit, date: buildTimestamp()}
	artifacts := t.buildRelease(identity)

	// Before the first upload, and deliberately so. The prefixes below are what
	// `install.sh` and the documented download URLs read, so a sync is
	// publication: past this line a binary that cannot name its release is
	// already the one people get, and refusing to attach it to the GitHub
	// release afterwards does not take it back. This is the last moment the
	// artifacts are private.
	if err := t.assertStampedIdentity(ctx, artifacts, identity); err != nil {
		return artifacts, fmt.Errorf("refusing to publish release %s: %w", version, err)
	}

	for _, prefix := range []string{
		path.Join(artifactPrefix, version),
		path.Join(artifactPrefix, "latest"),
	} {
		err := t.upload(ctx, &uploadOpts{
			artifacts:       artifacts,
			prefix:          prefix,
			endpoint:        endpoint,
			bucket:          bucket,
			accessKeyID:     accessKeyID,
			secretAccessKey: secretAccessKey,
		})
		if err != nil {
			return artifacts, fmt.Errorf("could not upload release artifacts to %s: %w", prefix, err)
		}
	}

	// The install script ships in the same call as the binaries it fetches.
	// It reads the checksum sidecars the prefixes above now carry, so an old
	// script pointed at new artifacts is a live bug, not a cosmetic one — and
	// a separate publish step is exactly the thing that gets skipped, fails
	// quietly after the binaries are already public, or never runs at all.
	// Folding it in here means one invocation either publishes binaries and
	// installer together or fails as a release: a cut cannot report success
	// while the served installer is stale.
	if err := t.uploadInstallScript(ctx, endpoint, bucket, accessKeyID, secretAccessKey); err != nil {
		return artifacts, fmt.Errorf("could not upload install script: %w", err)
	}

	versionDir := dag.Directory().WithNewFile("version", version+"\n")
	err := t.upload(ctx, &uploadOpts{
		artifacts:       versionDir,
		prefix:          path.Join(artifactPrefix, "latest"),
		endpoint:        endpoint,
		bucket:          bucket,
		accessKeyID:     accessKeyID,
		secretAccessKey: secretAccessKey,
	})
	if err != nil {
		return artifacts, fmt.Errorf("could not upload latest version file: %w", err)
	}

	return artifacts, nil
}

// Nightly builds and uploads nightly artifacts.
func (t *Tapesctl) Nightly(
	ctx context.Context,

	// Full commit the nightly is built from. It is what distinguishes one
	// nightly from the next, since they all carry the same version name.
	//
	// +optional
	// +default=""
	commit string,

	// Bucket endpoint URL.
	endpoint *dagger.Secret,

	// Bucket name.
	bucket *dagger.Secret,

	// Bucket access key ID.
	accessKeyID *dagger.Secret,

	// Bucket secret access key.
	secretAccessKey *dagger.Secret,
) (*dagger.Directory, error) {
	identity := stampedIdentity{version: nightlyVersion, commit: commit, date: buildTimestamp()}
	artifacts := t.buildRelease(identity)

	// Same ordering as ReleaseLatest, for a sharper version of the same reason:
	// the nightly prefix is a moving target that every nightly overwrites, so
	// the commit is the whole of what distinguishes tonight's from last
	// night's. A nightly published unable to name its commit is not a worse
	// nightly, it is not a nightly.
	if err := t.assertStampedIdentity(ctx, artifacts, identity); err != nil {
		return artifacts, fmt.Errorf("refusing to publish this nightly: %w", err)
	}

	err := t.upload(ctx, &uploadOpts{
		artifacts:       artifacts,
		prefix:          path.Join(artifactPrefix, nightlyVersion),
		endpoint:        endpoint,
		bucket:          bucket,
		accessKeyID:     accessKeyID,
		secretAccessKey: secretAccessKey,
	})

	return artifacts, err
}

// uploadInstallScript syncs install.sh to the object the download domain
// serves as the installer. ReleaseLatest calls it on every cut; UploadInstallSh
// exposes it for an out-of-band refresh.
func (t *Tapesctl) uploadInstallScript(
	ctx context.Context,
	endpoint *dagger.Secret,
	bucket *dagger.Secret,
	accessKeyID *dagger.Secret,
	secretAccessKey *dagger.Secret,
) error {
	installDir := dag.Directory().WithFile("install", t.Source.File("install.sh"))

	return t.upload(ctx, &uploadOpts{
		artifacts:       installDir,
		prefix:          artifactPrefix,
		endpoint:        endpoint,
		bucket:          bucket,
		accessKeyID:     accessKeyID,
		secretAccessKey: secretAccessKey,
	})
}

// UploadInstallSh uploads the install script under the tapesctl namespace.
//
// Releases do not need it: ReleaseLatest publishes the install script itself,
// so a cut cannot succeed while the served installer is stale. This remains a
// standalone function for republishing the script outside a release — say,
// after an installer-only fix that should not wait for the next cut.
func (t *Tapesctl) UploadInstallSh(
	ctx context.Context,

	// Bucket endpoint URL.
	endpoint *dagger.Secret,

	// Bucket name.
	bucket *dagger.Secret,

	// Bucket access key ID.
	accessKeyID *dagger.Secret,

	// Bucket secret access key.
	secretAccessKey *dagger.Secret,
) error {
	return t.uploadInstallScript(ctx, endpoint, bucket, accessKeyID, secretAccessKey)
}
