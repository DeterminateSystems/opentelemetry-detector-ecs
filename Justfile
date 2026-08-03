set shell := ["bash", "-euo", "pipefail", "-c"]

# List the recipes.
default:
    @just --list

# Run every check CI runs.
ci: fmt-check nixfmt-check editorconfig spelling clippy test doc unused audit package

# Format the Rust and Nix sources in place.
fmt:
    cargo fmt
    git ls-files '*.nix' | xargs nixfmt

# Check the Rust formatting.
fmt-check:
    cargo fmt --check

# Check the Nix formatting.
nixfmt-check:
    git ls-files '*.nix' | xargs nixfmt --check

# Check the tree against .editorconfig.
editorconfig:
    git ls-files | xargs eclint

# Check the spelling.
spelling:
    typos

# Lint with Clippy.
clippy:
    cargo clippy --all-targets --locked -- --deny warnings

# Run the tests.
test:
    cargo test --locked

# Build the documentation.
doc:
    RUSTDOCFLAGS="--deny warnings" cargo doc --no-deps --locked

# Report dependencies nothing uses.
unused:
    cargo machete

# Check the dependencies for security advisories.
audit:
    cargo audit --deny warnings

# Package the crate the way crates.io will.
package:
    cargo publish --dry-run --locked

# Publish the crate to crates.io.
publish:
    cargo publish --locked
