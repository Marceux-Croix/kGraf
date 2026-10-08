# Contributing to kGraf

Thank you for your interest in kGraf. Bug reports, ideas and pull requests are welcome.

## Before you start

- For a bug, open an [issue](https://github.com/Marceux-Croix/kGraf/issues/new/choose)
  with the steps to reproduce it and the output of `kgraf --version`.
- For a new feature or a large change, open an issue first so we can agree on the approach
  before you write the code.
- For a security problem, do not open a public issue. Follow [SECURITY.md](SECURITY.md).

## Build and test

You need [Rust](https://rustup.rs) 1.95 or later.

```bash
cargo build --workspace
```

Every pull request must pass the same gates as the main branch:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Dependency updates

No bot opens update pull requests, and no workflow starts on its own: updates are applied by
hand, about once a month and before each release.

```bash
cargo update --dry-run               # compatible updates; then cargo update to apply
cargo deny check advisories          # known security advisories in the lock file
```

Then run the three gates above. A major version bump, or a pinned GitHub Action SHA, is
updated one at a time in its own commit, after its release notes are read.

## Code conventions

- Rust 2021 edition or later, in the workspace layout.
- `thiserror` for library errors, `anyhow` at binary boundaries.
- No `unwrap` outside tests.
- Provider-specific logic stays in the adapter crates (`kgraf-provider-*`). It must not leak
  into the core crates.
- **Fail loud.** A substituted model, an ignored parameter or a missing dependency must
  raise an error, never degrade in silence.
- **Test the mechanism, not the setting.** A test must prove the effect, not only that a
  value was stored.
- Every model call goes through the accounting path. No provider call may bypass the ledger.
- UI changes follow [docs/STYLE_GUIDE.md](docs/STYLE_GUIDE.md). Cite a rule ID where a rule drives a choice.

Large design choices are recorded in [docs/decisions/](docs/decisions).

## Reusing code

Prefer existing, permissively licensed code over new code. When you copy or port code into
the tree, put it under `third_party/<name>/` with an `UPSTREAM` file that names the source,
the commit and the license, and update [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

## Pull requests

- Keep each pull request to one change, with a clear title.
- Add or update tests for the behavior you change.
- Update the docs that describe that behavior, such as [docs/USAGE.md](docs/USAGE.md).
- Fill in the pull request template.

## License

By contributing, you agree that your contributions are licensed under the
[Apache License, Version 2.0](LICENSE), as stated in section 5 of that license.

## Code of conduct

This project follows the [Contributor Covenant](CODE_OF_CONDUCT.md). By taking part, you
agree to uphold it.
