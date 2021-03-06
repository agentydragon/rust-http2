#!/bin/sh -e

set -x

export RUST_BACKTRACE=1

rustc --version

if test "$ACTION" = "h2spec"; then
    ci/install-h2spec.sh
    export PATH="$PATH:$(pwd)"
    cargo run --manifest-path h2spec-test/Cargo.toml --bin the_test
else
    # Something doesn't work here, but we need to install openssl
    if test -n "$ON_WINDOWS"; then
        $msys2 pacman -S openssl-devel pkg-config
    fi

    # Use one thread for better errors
    cargo test --all --all-targets -- --test-threads=1

    # `--all-targets` does not include doctests
    # https://github.com/rust-lang/cargo/issues/6669
    cargo test --doc

    # Check the docs
    cargo doc
fi

# vim: set ts=4 sw=4 et:
