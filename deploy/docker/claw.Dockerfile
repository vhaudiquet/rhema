# rhema-claw: the resident forge-watching daemon.
# Build from the repo root (the context is the repo):
#   docker build -f deploy/docker/claw.Dockerfile .

# The builder tag tracks rust-toolchain.toml: the pinned channel is what
# every commit is tested against. The full image (not -slim) carries the C
# toolchain rusqlite's bundled SQLite build needs.
FROM rust:1.98.1 AS builder
WORKDIR /build

# Manifests first: the registry cache this populates survives source edits.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates/rhema-acp/Cargo.toml crates/rhema-acp/Cargo.toml
COPY crates/rhema-autonomous-agent/Cargo.toml crates/rhema-autonomous-agent/Cargo.toml
COPY crates/rhema-claw/Cargo.toml crates/rhema-claw/Cargo.toml
COPY crates/rhema-code/Cargo.toml crates/rhema-code/Cargo.toml
COPY crates/rhema-coding-agent/Cargo.toml crates/rhema-coding-agent/Cargo.toml
COPY crates/rhema-config/Cargo.toml crates/rhema-config/Cargo.toml
COPY crates/rhema-core/Cargo.toml crates/rhema-core/Cargo.toml
COPY crates/rhema-memory/Cargo.toml crates/rhema-memory/Cargo.toml
COPY crates/rhema-provider/Cargo.toml crates/rhema-provider/Cargo.toml
COPY crates/rhema-provider-anthropic/Cargo.toml crates/rhema-provider-anthropic/Cargo.toml
COPY crates/rhema-provider-copilot/Cargo.toml crates/rhema-provider-copilot/Cargo.toml
COPY crates/rhema-provider-google/Cargo.toml crates/rhema-provider-google/Cargo.toml
COPY crates/rhema-provider-llama-cpp/Cargo.toml crates/rhema-provider-llama-cpp/Cargo.toml
COPY crates/rhema-provider-openai-compatible/Cargo.toml crates/rhema-provider-openai-compatible/Cargo.toml
COPY crates/rhema-provider-openrouter/Cargo.toml crates/rhema-provider-openrouter/Cargo.toml
COPY crates/rhema-rsd/Cargo.toml crates/rhema-rsd/Cargo.toml
COPY integrations/rhema-forge-integration/Cargo.toml integrations/rhema-forge-integration/Cargo.toml

# Resolution reads the vendored crossterm (the root manifest path-patches
# to it) even though the image builds the daemon, which does not link it.
COPY vendor/ vendor/

# Cargo validates explicit target
# paths at manifest-parse time, so the fetch needs stub files at every
# declared target path: src/ for every member, plus examples/tests/benches
# as declared (default path from the target name, or the explicit `path`).
# The loop drives off the crates/*/ and integrations/*/ globs because
# neither glob's */src matches anything until the stub exists — an
# unmatched glob would hand mkdir a literal path. The real sources land
# below. (The awk program must stay on one line: a Dockerfile line not
# ending in a backslash ends the RUN.)
RUN for crate in crates/*/ integrations/*/; do \
           mkdir -p "$crate/src"; \
           touch "$crate/src/lib.rs" "$crate/src/main.rs"; \
           awk -F'"' '/^\[/ { d = ""; p = 0; if ($0 == "[[example]]") { d = "examples" } else if ($0 == "[[test]]") { d = "tests" } else if ($0 == "[[bench]]") { d = "benches" }; next } d == "" { next } /^name = / && p == 0 { print d "/" $2 ".rs" } /^path = / { p = 1; print $2 }' "$crate/Cargo.toml" \
           | while IFS= read -r target; do \
                 mkdir -p "$crate$(dirname "$target")"; \
                 touch "$crate$target"; \
             done; \
       done \
    && cargo fetch

# Sources. target/ and .git never land here: the repo-root .dockerignore
# keeps them out of the context.
COPY . .
RUN cargo build --release -p rhema-claw

FROM debian:stable-slim

# bash and git are load-bearing, not conveniences: the coding agent shells
# out via `bash -c` and clones/fetches in its per-thread workspaces.
RUN apt-get update \
    && apt-get install -y --no-install-recommends bash git ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Numeric USER: Kubernetes cannot verify runAsNonRoot against a name.
RUN useradd --uid 1000 --user-group --create-home claw \
    && install -d -o 1000 -g 1000 /config /data

COPY --from=builder /build/target/release/rhema-claw /usr/local/bin/rhema-claw

# The chart passes --config /config/claw.yaml (the ConfigMap mount) and
# mounts the state PVC at /data; these defaults serve a bare `docker run`.
ENV XDG_CONFIG_HOME=/config \
    XDG_STATE_HOME=/data/state
VOLUME /data
EXPOSE 7799

USER 1000:1000
ENTRYPOINT ["/usr/local/bin/rhema-claw"]
