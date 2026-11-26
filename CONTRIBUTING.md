# Contributing to redis-rover

Thanks for your interest in rrover. Bug reports, ideas, docs fixes and code are all welcome. Everyone taking part agrees to the [code of conduct](CODE_OF_CONDUCT.md).

## Ways to help

- **Report a bug.** Open a [bug report](https://github.com/danik-tro/redis-rover/issues/new?template=bug_report.yml). The form asks for `rrover --version`, your OS and terminal, and the server (`rrover --probe` prints it).
- **Suggest a feature.** Open a [feature request](https://github.com/danik-tro/redis-rover/issues/new?template=feature_request.yml) and describe the problem you want to solve first; the shape of the feature can come later.
- **Improve the docs.** The README, the [guide](docs/guide.md) and the help texts. A typo fix needs no issue first.
- **Send code.** For anything larger than a small fix, open an issue first, so we can agree on the approach before you spend time on it.
- **Security problems** go through a private report, never a public issue. See [SECURITY.md](SECURITY.md).

## Development

**Prerequisites:**

- Rust 1.97 (pinned in `rust-toolchain.toml`, so `rustup` picks it up)
- A Redis on `localhost:6379` to run the app against
- Docker, for the integration tests only

**Common commands:**

```bash
# Build and run (against localhost:6379)
cargo build
cargo run

# Tests (no Docker or Redis needed)
cargo test

# Integration tests against real Redis servers in Docker
cargo test -- --ignored --test-threads=1

# Lint and formatting, as CI runs them
cargo clippy --all-targets --all-features -- -D warnings -W clippy::all -W clippy::pedantic
cargo fmt --check
```

**Logs.** rrover writes `redis-rover.log` in its data directory (`rrover --version` prints where). For more detail, run with `RUST_LOG=redis_rover=debug`.

### Tests

- **Unit tests** sit next to the code and run with `cargo test`.
- **Integration tests** (`tests/integration.rs`) start `redis:7-alpine` with [testcontainers](https://crates.io/crates/testcontainers), one container per test, so they never touch your own Redis. `tests/cluster.rs` and `tests/sentinel.rs` do the same for Cluster and Sentinel, on fixed ports, so run them one test at a time.
- **Golden frames** (`tests/golden.rs`) draw screens from fixed data and compare their text with `tests/golden/<name>.txt`. After an intended UI change, rewrite them with `UPDATE_GOLDEN=1 cargo test --test golden` and check the diff of the `.txt` files in your PR.
- **Keybindings reference.** After changing a default binding or a help label, regenerate `docs/keybindings.md` with `UPDATE_DOCS=1 cargo test keybindings_reference`.

### Benchmarks

If a change touches the keyspace or rendering, run the benches before and after:

```bash
cargo bench --bench keyspace
cargo bench --bench render
scripts/bench.sh --quick    # writes target/bench-summary.md
```

Compare with [`benches/baseline.md`](benches/baseline.md). A regression over 10 % needs an explanation in the PR. `scripts/bench.sh` without `--quick` also runs the 10M-key case, which takes a few minutes and about 2 GB of RAM.

### Pre-commit hooks

The repo uses [prek](https://github.com/j178/prek), a drop-in for the `pre-commit` framework, to run `cargo fmt`, `cargo clippy` and file checks before every commit:

```bash
brew install prek          # or: cargo install prek
prek install               # install the git hooks
prek run --all-files       # run them by hand
```

The hooks also refuse files over 500 KB, so keep images and GIFs small.

## Pull requests

1. Fork the repository and create a branch from `master`.
2. Make your change, with tests for new behaviour or a fixed bug.
3. Run `prek run --all-files` and `cargo test`.
4. Add a line to [CHANGELOG.md](CHANGELOG.md) if users will notice the change.
5. Open a pull request and fill in the template. CI runs fmt, clippy, the build, the tests (nextest) and the Docker integration suites.

Keep each PR to one concern and each commit focused. Commit messages follow a short `type: summary` form, as in the history: `feat:`, `fix:`, `docs:`, `test:`, `refactor:`, `perf:`, `build:`, `ci:`, `chore:`.

The crate has a library target only so that benches and tests can reach internal types. It is not a public API, so there is no need to keep it stable.

## License

By contributing, you agree that your contributions are licensed under the [MIT License](LICENSE).
