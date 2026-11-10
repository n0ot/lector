FROM ubuntu:24.04

ARG DEBIAN_FRONTEND=noninteractive
ARG NEXTEST_VERSION=0.9.143

RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        bash \
        build-essential \
        ca-certificates \
        curl \
        file \
        git \
        ncurses-bin \
        perl \
        pkg-config \
        tmux \
        xz-utils \
    && rm -rf /var/lib/apt/lists/*

ENV CARGO_HOME=/opt/cargo \
    LANG=C.UTF-8 \
    LC_ALL=C.UTF-8 \
    PATH=/opt/cargo/bin:${PATH} \
    RUSTUP_HOME=/opt/rustup

COPY rust-toolchain.toml /opt/lector/rust-toolchain.toml
COPY scripts/check-rust-toolchain /opt/lector/scripts/check-rust-toolchain
WORKDIR /opt/lector

RUN curl --proto '=https' --tlsv1.2 --fail --silent --show-error \
        https://sh.rustup.rs \
        | sh -s -- --default-toolchain "$(scripts/check-rust-toolchain --channel)" --profile minimal --no-modify-path -y \
    && scripts/check-rust-toolchain \
    && cargo install cargo-nextest --version "${NEXTEST_VERSION}" --locked

WORKDIR /work
