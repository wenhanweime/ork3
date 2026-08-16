set shell := ["zsh", "-cu"]

default:
    @just --list

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

check:
    cargo fmt --all -- --check
    cargo clippy --locked --all-targets -- -D warnings
    cargo test --locked -- --test-threads=1

build:
    cargo build --locked

release:
    cargo build --release --locked

install:
    cargo install --path . --locked
