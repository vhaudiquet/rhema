# rhema-sandboxd: the sandbox wire protocol served from a host.
# Build from the repo root (the context is the repo):
#   docker build -f deploy/docker/sandboxd.Dockerfile .

# The builder tag tracks rust-toolchain.toml: the pinned channel is what
# every commit is tested against. The full image (not -slim) keeps this
# builder identical to claw.Dockerfile's, though the graph is pure Rust:
# the bundled SQLite of rhema-identity stays out of `-p rhema-sandbox`'s
# dependency closure.
FROM rust:1.98.1 AS builder
WORKDIR /build

# Manifests first: the registry cache this populates survives source edits.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates/rhema-acp/Cargo.toml crates/rhema-acp/Cargo.toml
COPY crates/rhema-agent-tools/Cargo.toml crates/rhema-agent-tools/Cargo.toml
COPY crates/rhema-autonomous-agent/Cargo.toml crates/rhema-autonomous-agent/Cargo.toml
COPY crates/rhema-claw/Cargo.toml crates/rhema-claw/Cargo.toml
COPY crates/rhema-cloud-agent/Cargo.toml crates/rhema-cloud-agent/Cargo.toml
COPY crates/rhema-cloudflare-code/Cargo.toml crates/rhema-cloudflare-code/Cargo.toml
COPY crates/rhema-code/Cargo.toml crates/rhema-code/Cargo.toml
COPY crates/rhema-coding-agent/Cargo.toml crates/rhema-coding-agent/Cargo.toml
COPY crates/rhema-config/Cargo.toml crates/rhema-config/Cargo.toml
COPY crates/rhema-core/Cargo.toml crates/rhema-core/Cargo.toml
COPY crates/rhema-identity/Cargo.toml crates/rhema-identity/Cargo.toml
COPY crates/rhema-memory/Cargo.toml crates/rhema-memory/Cargo.toml
COPY crates/rhema-provider/Cargo.toml crates/rhema-provider/Cargo.toml
COPY crates/rhema-provider-anthropic/Cargo.toml crates/rhema-provider-anthropic/Cargo.toml
COPY crates/rhema-provider-copilot/Cargo.toml crates/rhema-provider-copilot/Cargo.toml
COPY crates/rhema-provider-google/Cargo.toml crates/rhema-provider-google/Cargo.toml
COPY crates/rhema-provider-llama-cpp/Cargo.toml crates/rhema-provider-llama-cpp/Cargo.toml
COPY crates/rhema-provider-openai-compatible/Cargo.toml crates/rhema-provider-openai-compatible/Cargo.toml
COPY crates/rhema-provider-openrouter/Cargo.toml crates/rhema-provider-openrouter/Cargo.toml
COPY crates/rhema-rsd/Cargo.toml crates/rhema-rsd/Cargo.toml
COPY crates/rhema-sandbox/Cargo.toml crates/rhema-sandbox/Cargo.toml
COPY crates/rhema-subagents/Cargo.toml crates/rhema-subagents/Cargo.toml
COPY crates/rhema-transcript/Cargo.toml crates/rhema-transcript/Cargo.toml
COPY crates/rhema-vercel-agent/Cargo.toml crates/rhema-vercel-agent/Cargo.toml
COPY integrations/rhema-calendar-integration/Cargo.toml integrations/rhema-calendar-integration/Cargo.toml
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
#
# No src/main.rs stub: most members have no default binary, the stub
# would survive `COPY . .`, and the phantom autobin it creates fails
# E0601 the moment that member is the `-p` package (rhema-sandbox
# itself). Declared [[bin]] paths are never stubbed either — the awk
# stubs examples/tests/benches only — and fetch needs no bin to exist.
RUN for crate in crates/*/ integrations/*/; do \
           mkdir -p "$crate/src"; \
           touch "$crate/src/lib.rs"; \
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

# Feature `server` only: the daemon needs no local/mock/client code
# (server implies the local backend and tokio's rt/net, per the
# crate's feature graph).
RUN cargo build --release --locked -p rhema-sandbox --features server

FROM debian:stable-slim

# coreutils — the `timeout` prefix the exec route wraps every command
# in — ships with debian-slim; bash does not. git and the CA store are
# load-bearing too: a `{git}` seed is a `git clone` (tarball seeds need
# neither).
RUN apt-get update \
    && apt-get install -y --no-install-recommends bash git ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Numeric USER: Kubernetes cannot verify runAsNonRoot against a name.
RUN useradd --uid 1000 --user-group --create-home sandboxd \
    && install -d -o 1000 -g 1000 /var/lib/rhema-sandboxd

COPY --from=builder /build/target/release/rhema-sandboxd /usr/local/bin/rhema-sandboxd

# The base and the bind host are fixed to the image — the binary's
# default host is loopback, unreachable through a published port, and
# `docker run` arguments replace CMD wholesale, so a per-run default
# would never survive the always-present `--token`. The token and port
# are per-run flags appended to the entrypoint (`--token` has no
# environment fallback — the runner passes it). No EXPOSE: `--port`
# defaults to 0 (OS-picked), so the image fixes no port.
VOLUME /var/lib/rhema-sandboxd
USER 1000:1000
ENTRYPOINT ["/usr/local/bin/rhema-sandboxd", "--base", "/var/lib/rhema-sandboxd", "--host", "0.0.0.0"]
