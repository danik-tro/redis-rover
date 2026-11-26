## What this changes

<!-- What the PR does, and why. Link the issue it fixes, e.g. "Fixes #123". -->

## How it was tested

<!-- The tests you added or ran, and anything you checked by hand: the server, the terminal, the screen. -->

## Checklist

- [ ] `cargo fmt --check` and `cargo clippy --all-targets --all-features -- -D warnings -W clippy::all -W clippy::pedantic` pass
- [ ] `cargo test` passes, with tests for new behaviour or the fixed bug
- [ ] After a UI change: golden frames updated (`UPDATE_GOLDEN=1 cargo test --test golden`) and their diff checked
- [ ] After changing a default key or a help label: `UPDATE_DOCS=1 cargo test keybindings_reference`
- [ ] Docs (README, `docs/guide.md`) and `CHANGELOG.md` updated if users will notice the change
- [ ] For keyspace or rendering changes: benches compared with `benches/baseline.md`

<!-- Screenshots or a short recording help for UI changes. -->
