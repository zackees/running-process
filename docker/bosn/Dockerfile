# syntax=docker/dockerfile:1.7
#
# Managed Linux stack for Bosn. This is deliberately separate from
# docker/dev/Dockerfile so the shared interactive developer image can evolve
# independently. Keep this Rust image aligned with the workspace
# rust-toolchain.toml (currently 1.95.0).

FROM rust:1.95.0-bookworm

# Native and Python dependencies used by the repository's Linux test and
# lint entrypoints. The protocol package owns protobuf generation.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        curl \
        gdb \
        git \
        libssl-dev \
        pkg-config \
        protobuf-compiler \
        python3 \
        python3-dev \
        python3-pip \
        python3-venv \
        zstd \
 && rm -rf /var/lib/apt/lists/*

# The repository's Python entrypoints use uv. Install it outside the mounted
# toolchain directory so it remains available when Bosn attaches its volumes.
RUN curl -LsSf https://astral.sh/uv/install.sh | env UV_INSTALL_DIR=/usr/local/bin sh

# The official pinned image provides the selected compiler. Bosn mounts the
# toolchain directories as persistent volumes, which Docker seeds from this
# image on first use. Ensure the managed lint components are seeded too.
RUN rustup component add clippy rustfmt

ENV CARGO_TARGET_DIR=/work/target
ENV UV_PROJECT_ENVIRONMENT=/uv/venv
ENV UV_CACHE_DIR=/uv/cache
ENV PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1 \
    PIP_DISABLE_PIP_VERSION_CHECK=1
ENV RUNNING_PROCESS_LINT_COMMAND_TIMEOUT_SECONDS=""

# Container roots do not supply these session files. Tests and diagnostics use
# them to derive compatible broker and watchdog runtime locations.
RUN cat /proc/sys/kernel/random/uuid | tr -d '-' > /etc/machine-id
ENV XDG_RUNTIME_DIR=/run/user/0
RUN mkdir -p /run/user/0 && chmod 700 /run/user/0

WORKDIR /work
CMD ["bash"]
