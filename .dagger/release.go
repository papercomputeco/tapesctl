package main

import (
	"context"
	"fmt"
	"path"

	"dagger/tapesctl/internal/dagger"
)

const artifactPrefix = "tapesctl"

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

	// Version string, for example v1.0.0.
	version string,

	// Bucket endpoint URL.
	endpoint *dagger.Secret,

	// Bucket name.
	bucket *dagger.Secret,

	// Bucket access key ID.
	accessKeyID *dagger.Secret,

	// Bucket secret access key.
	secretAccessKey *dagger.Secret,
) (*dagger.Directory, error) {
	artifacts := t.BuildRelease(ctx)

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

	// Bucket endpoint URL.
	endpoint *dagger.Secret,

	// Bucket name.
	bucket *dagger.Secret,

	// Bucket access key ID.
	accessKeyID *dagger.Secret,

	// Bucket secret access key.
	secretAccessKey *dagger.Secret,
) (*dagger.Directory, error) {
	artifacts := t.BuildRelease(ctx)
	err := t.upload(ctx, &uploadOpts{
		artifacts:       artifacts,
		prefix:          path.Join(artifactPrefix, "nightly"),
		endpoint:        endpoint,
		bucket:          bucket,
		accessKeyID:     accessKeyID,
		secretAccessKey: secretAccessKey,
	})

	return artifacts, err
}

// UploadInstallSh uploads the install script under the tapesctl namespace.
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
