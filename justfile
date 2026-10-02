default:
    @just --list

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all --check

lint:
    cargo clippy --all-targets -- -D warnings

build:
    cargo build --release

ci: fmt-check lint build
