# Based around the auto-documented Makefile:
# http://marmelab.com/blog/2016/02/29/auto-documented-makefile.html

VERSION ?= $(shell git describe --tags --always --dirty 2>/dev/null || echo dev)
GOBIN ?= $(shell go env GOBIN)

ifeq ($(strip $(GOBIN)),)
GOBIN := $(shell go env GOPATH)/bin
endif

LDFLAGS := -s -w

.PHONY: check
check: ## Runs all Dagger checks. Auto-fixes are not automatically applied.
	$(call print-target)
	dagger check

.PHONY: format
format: ## Runs golangci-lint linters and formatters with auto-fixes applied.
	$(call print-target)
	dagger call fix-lint export --path .

.PHONY: build-local
build-local: ## Builds local artifacts with the local Go toolchain.
	$(call print-target)
	@mkdir -p ./build
	CGO_ENABLED=0 go build -ldflags "$(LDFLAGS)" -o ./build/tapesctl ./cli/tapesctl

.PHONY: install
install: build-local ## Builds and installs tapesctl to GOBIN.
	$(call print-target)
	@mkdir -p $(GOBIN)
	# install writes a temp file and renames it into place, avoiding in-place
	# executable replacement issues on macOS.
	install -m 0755 ./build/tapesctl $(GOBIN)/tapesctl

.PHONY: build
build: ## Builds all cross-platform release artifacts.
	$(call print-target)
	dagger call build-release export --path ./build

.PHONY: nightly
nightly: ## Builds and uploads nightly tapesctl artifacts.
	dagger call \
		nightly \
			--endpoint=env://BUCKET_ENDPOINT \
			--bucket=env://BUCKET_NAME \
			--access-key-id=env://BUCKET_ACCESS_KEY_ID \
			--secret-access-key=env://BUCKET_SECRET_ACCESS_KEY

.PHONY: upload-install-script
upload-install-script: ## Uploads the tapesctl install script.
	dagger call \
		upload-install-sh \
			--endpoint=env://BUCKET_ENDPOINT \
			--bucket=env://BUCKET_NAME \
			--access-key-id=env://BUCKET_ACCESS_KEY_ID \
			--secret-access-key=env://BUCKET_SECRET_ACCESS_KEY

.PHONY: release
release: ## Builds and uploads tapesctl release artifacts.
	dagger call \
		release-latest \
			--version=$(VERSION) \
			--endpoint=env://BUCKET_ENDPOINT \
			--bucket=env://BUCKET_NAME \
			--access-key-id=env://BUCKET_ACCESS_KEY_ID \
			--secret-access-key=env://BUCKET_SECRET_ACCESS_KEY

.PHONY: clean
clean: ## Removes built artifacts.
	$(call print-target)
	@rm -rf ./build

.PHONY: test
test: ## Runs tests through Dagger.
	$(call print-target)
	dagger call test

.PHONY: help
.DEFAULT_GOAL := help
help: ## Prints this help message.
	@grep -h -E '^[a-zA-Z0-9_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-30s\033[0m %s\n", $$1, $$2}'

define print-target
    @printf "Executing target: \033[36m$@\033[0m\n"
endef
