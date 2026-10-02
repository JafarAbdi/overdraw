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

dist:
    cargo build --release --locked --target x86_64-unknown-linux-musl

ci: fmt-check lint build
