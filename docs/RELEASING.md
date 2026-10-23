# Release checklist

Infernaut is distributed from GitHub. Crates.io publication is disabled in
`Cargo.toml` until the maintainers deliberately choose to support it.

## First repository publication

1. Create the GitHub repository privately.
2. Set `repository`, `homepage`, and, if applicable, `documentation` in
   `Cargo.toml` to the final URL.
3. Add changelog comparison links only after that URL exists.
4. Confirm that `git status --ignored --short` marks `.env`,
   `infernaut.toml`, `infernaut.toml.bak-*`, `sniffer.toml`, and `target/`
   as ignored.
5. Inspect `git add --dry-run .` before staging. It must not list credentials,
   operator configuration, configuration backups, captures, logs, model files,
   or internal infrastructure details.
6. Run a dedicated secret scanner against the intended commit and review every
   finding. Synthetic test credentials are allowed only in local fixtures.
7. Complete the validation commands below.
8. Push the initial commit, inspect the rendered repository on GitHub, and
   invite a small group of clean-machine testers before making it public.

## Validation

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features --locked
cargo +1.89.0 check --all-targets --all-features --locked
bash tests/e2e.sh
cargo package --locked
docker compose config --quiet
docker compose build
```

Also verify the example configuration from a clean checkout:

```bash
cp .env.example .env
cp infernaut.example.toml infernaut.toml
# Add a temporary INFERNAUT_API_KEY to .env.
cargo run -- config validate infernaut.toml
```

The minimum-supported-Rust job in CI must pass before release. The macOS job is
particularly important because LM Studio and MLX are core homelab use cases.

Run a dependency advisory scan (for example, `cargo audit`) in addition to the
secret scan. Test Docker startup, authentication, config-TUI save/restart, and
SIGTERM draining with a synthetic provider before distributing the image.

## Versioned release

1. Move user-visible entries from `Unreleased` into a versioned changelog
   section with the release date.
2. Update the package version in `Cargo.toml` and refresh `Cargo.lock`.
3. Rerun the complete validation suite from a clean checkout.
4. Commit the release, create an annotated `vMAJOR.MINOR.PATCH` tag, and push
   both the commit and tag.
5. Publish release notes that include upgrade notes, configuration migrations,
   compatibility changes, known limitations, and checksums for any attached
   binaries.

Do not rebuild or restart an operator's running gateway merely to prepare a
source release. Validate release artifacts in a separate checkout or CI.
