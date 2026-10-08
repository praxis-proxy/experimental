# Praxis Experimental

Experimental filters, features, and builds for Praxis.

Check out our [enhancements process] before using this repository.

This repository is intentionally a proving ground. Features may change or be
removed before they are promoted to a released Praxis repository.

[enhancements process]:https://github.com/praxis-proxy/enhancements

## Development

Read [CONTRIBUTING.md](CONTRIBUTING.md) and [the development conventions](docs/conventions.md)
before making changes.

```console
make all
```

The repository enforces formatting, strict Rust and Clippy lints, tests,
documentation, dependency auditing, and supply-chain checks. See
[docs/development.md](docs/development.md) for prerequisites and individual
commands.

## Standalone authentication

[Manual JWT authentication](docs/manual-jwt.md) provides administrator-issued
credentials, per-user revocation and a private dashboard login callback.

## Demos

See [demos/README.md](demos/README.md) for the index of experimental demos.

## Media and Large Files

Do not commit videos or other large binary media to this repository. They bloat
the git history for everyone who clones or fetches, and git cannot compress
already-compressed formats like MP4.

Instead, upload media as GitHub artifacts (e.g. drag files into markdown editor
on the web, or use a release asset, or a workflow artifact) and link them from
the relevant documentation.

## License

[Apache-2.0](LICENSE)
