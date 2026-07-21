# Contributing

The Nix flake dev shell is the recommended development environment. It pins Go
1.25 and Dagger.

```bash
nix develop
make build-local
./build/tapesctl
```

Before opening a pull request, run:

```bash
make format
make check
```

Pull request titles must use one of the repository's accepted contribution
labels, such as `✨ feat:`, `🔧 fix:`, `🧹 chore:`, or `📚 docs:`.
