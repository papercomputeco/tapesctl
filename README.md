# tapesctl

The command-line client for Tapes.

## Install

```bash
curl -sSfL https://download.tapes.dev/tapesctl/install | bash
```

Set `TAPESCTL_VERSION` to install a specific release or nightly build, and
`TAPESCTL_INSTALL_DIR` to override `/usr/local/bin`.

## Develop

```bash
nix develop
make build-local
./build/tapesctl
```

Run `make help` to see all development and release operations.
