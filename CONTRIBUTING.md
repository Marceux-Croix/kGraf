# Contributing to kGraf

Thank you for your interest in kGraf. Bug reports, ideas and pull requests are welcome.

## Before you start

- For a bug, open an [issue](https://github.com/Marceux-Croix/kGraf/issues/new/choose)
  with the steps to reproduce it, the output of `kgraf --version`, and the git commit hash
  you built from (`git rev-parse HEAD`).
- For a new feature or a large change, open an issue first so we can agree on the approach
  before you write the code.
- For a security problem, do not open a public issue. Follow [SECURITY.md](SECURITY.md).

## Build and test

You need [Rust](https://rustup.rs). `rustup` installs the toolchain version the repository
pins (`rust-toolchain.toml`) on the first build.

```bash
cargo build --workspace
```

Run these three gates locally before you open a pull request. No workflow starts on its
own. The maintainer runs CI after pulling your change into a branch of this repository.

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

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

## Verifying a route

A route is VERIFIED in [docs/MODEL-SUPPORT.md](docs/MODEL-SUPPORT.md) only after one real
request and response through the adapter, built as a run builds it.

1. Set the route's credential in your shell.
2. For a generative route, run `kgraf call --provider <route> --model <model-id> --record
   <file>`. For another kind, use the kind's tool in a run, or an ignored live test such as
   `crates/kgraf-fleet/tests/live_embeddings.rs`.
3. Check the model echo, the reply, the raw usage and the ledger row.
4. Record the date, the model, the request as sent, the raw reply and what it showed.
5. Change the route's confidence in the catalog and update the table.

A route that fails the check stays BUILT, UNTESTED, and the adapter is fixed first.

## Pull requests

- Keep each pull request to one change, with a clear title.
- Add or update tests for the behavior you change.
- Update the docs that describe that behavior, such as [docs/USAGE.md](docs/USAGE.md).
- Fill in the pull request template.

## Maintainers

[docs/LINUX-TESTING.md](docs/LINUX-TESTING.md) describes the Linux test container.

## License

By contributing, you agree that your contributions are licensed under the
[Apache License, Version 2.0](LICENSE), as stated in section 5 of that license.

## Code of conduct

This project follows the [Contributor Covenant](CODE_OF_CONDUCT.md). By taking part, you
agree to uphold it.
