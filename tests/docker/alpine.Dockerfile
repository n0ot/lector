FROM alpine:3.24

RUN apk add --no-cache \
    bash binutils build-base ca-certificates curl file git pkgconf tar xz

ENV CARGO_HOME=/opt/cargo \
    RUSTUP_HOME=/opt/rustup \
    PATH=/opt/cargo/bin:${PATH}

COPY rust-toolchain.toml /opt/lector/rust-toolchain.toml
COPY scripts/check-rust-toolchain /opt/lector/scripts/check-rust-toolchain
WORKDIR /opt/lector

RUN curl --proto '=https' --tlsv1.2 --fail --silent --show-error \
        https://sh.rustup.rs \
        | sh -s -- --default-toolchain "$(scripts/check-rust-toolchain --channel)" --profile minimal --no-modify-path -y \
    && scripts/check-rust-toolchain

WORKDIR /work
