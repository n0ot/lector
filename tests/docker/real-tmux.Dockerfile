FROM ubuntu:24.04

ARG DEBIAN_FRONTEND=noninteractive
ARG RUST_VERSION=1.97.0
ARG TARGETARCH

RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        bash \
        build-essential \
        ca-certificates \
        curl \
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
    RUSTUP_HOME=/opt/rustup \
    RUSTUP_TOOLCHAIN=${RUST_VERSION}

RUN curl --proto '=https' --tlsv1.2 --fail --silent --show-error \
        https://sh.rustup.rs \
        | sh -s -- --default-toolchain "${RUST_VERSION}" --profile minimal --no-modify-path -y

COPY install-nextest.sh /tmp/install-nextest.sh
RUN bash /tmp/install-nextest.sh "${TARGETARCH}" && rm /tmp/install-nextest.sh

WORKDIR /work
