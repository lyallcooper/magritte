# Magritte

> *Ceci n'est pas Magit.*

A fast, keyboard-first git client in the spirit of [Magit](https://magit.vc/)—no
Emacs required.

### See [magritte.lyall.co](https://magritte.lyall.co) for installation, usage, and configuration

## Develop

The workspace uses Rust 1.96, pinned in [`.mise.toml`](.mise.toml). Magritte
invokes the `git` executable rather than linking to libgit2.

```sh
mise install                 # optional, uses the pinned toolchain
cargo run --release -- .     # build and run on a repo
cargo test
cargo clippy --all-targets
cargo fmt --check
```

The first build takes longer because it compiles GPUI and the pinned
dependencies. Later builds are incremental.

The workspace has two crates:

- `magritte-core` contains synchronous, UI-independent Git operations.
- `magritte` contains the GPUI app, background work, and cancellation.

Read [AGENTS.md](AGENTS.md) for repository conventions. The website and
rendered docs live in [`site/`](site/README.md), and
[Magit parity](docs/dev/magit-parity.md) tracks supported and missing Magit
features in detail.

## License

Magritte is dual-licensed under [MIT](LICENSE-MIT) or
[Apache 2.0](LICENSE-APACHE), at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in Magritte by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
