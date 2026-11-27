# The program status lane builds an immutable upstream revision. The default
# target continues to use Ubuntu's tmux.
FROM ubuntu:24.04 AS tmux-preview-build

RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        autoconf automake bison build-essential ca-certificates curl \
        libevent-dev libncurses-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /tmp/tmux
RUN curl --proto '=https' --tlsv1.2 --fail --silent --show-error --location \
        https://codeload.github.com/tmux/tmux/tar.gz/61aded536b925f6eadfd37457dc765c85116f51a \
        --output /tmp/tmux.tar.gz \
    && tar --extract --gzip --file /tmp/tmux.tar.gz --strip-components=1 \
    && sh autogen.sh \
    && ./configure \
    && make -j"$(nproc)"

FROM ubuntu:24.04 AS base

ARG DEBIAN_FRONTEND=noninteractive
ARG TARGETARCH

RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        bash \
        binutils \
        build-essential \
        ca-certificates \
        curl \
        file \
        git \
        ncurses-bin \
        perl \
        pkg-config \
        python3 \
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
    && case "$TARGETARCH" in \
        amd64) rustup target add x86_64-unknown-linux-musl ;; \
        arm64) rustup target add aarch64-unknown-linux-musl ;; \
        *) exit 2 ;; \
    esac

COPY tests/docker/install-nextest.sh /tmp/install-nextest.sh
RUN bash /tmp/install-nextest.sh "${TARGETARCH}" && rm /tmp/install-nextest.sh

WORKDIR /work


FROM base AS program-status-preview
COPY --from=tmux-preview-build /tmp/tmux/tmux /usr/local/bin/tmux

FROM base AS distro
