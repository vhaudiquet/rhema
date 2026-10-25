# rhema-sandboxd: the sandbox wire protocol served from a host.
# Build from the repo root (the context is the repo):
#   docker build -f deploy/docker/sandboxd.Dockerfile .

# The builder tag tracks rust-toolchain.toml: the pinned channel is what
# every commit is tested against. The full image (not -slim) keeps this
# builder identical to claw.Dockerfile's, though the graph is pure Rust:
# the bundled SQLite of rhema-identity stays out of the daemon
# crate's dependency closure.
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
COPY crates/rhema-sandbox-docker/Cargo.toml crates/rhema-sandbox-docker/Cargo.toml
COPY crates/rhema-sandbox-http/Cargo.toml crates/rhema-sandbox-http/Cargo.toml
COPY crates/rhema-sandbox-local/Cargo.toml crates/rhema-sandbox-local/Cargo.toml
COPY crates/rhema-sandbox-server/Cargo.toml crates/rhema-sandbox-server/Cargo.toml
COPY crates/rhema-sandbox-ssh/Cargo.toml crates/rhema-sandbox-ssh/Cargo.toml
COPY crates/rhema-subagents/Cargo.toml crates/rhema-subagents/Cargo.toml
COPY crates/rhema-transcript/Cargo.toml crates/rhema-transcript/Cargo.toml
COPY crates/rhema-vercel-agent/Cargo.toml crates/rhema-vercel-agent/Cargo.toml
COPY integrations/rhema-calendar-integration/Cargo.toml integrations/rhema-calendar-integration/Cargo.toml
COPY integrations/rhema-forge-integration/Cargo.toml integrations/rhema-forge-integration/Cargo.toml
# rhema-claw path-depends on the tools contract, which lives in its own
# workspace: the member manifests inherit from that root, so the root
# rides along with them.
COPY tools/Cargo.toml tools/Cargo.toml
COPY tools/contract/Cargo.toml tools/contract/Cargo.toml
COPY tools/common/Cargo.toml tools/common/Cargo.toml
COPY tools/notify/Cargo.toml tools/notify/Cargo.toml


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
# E0601 the moment that member is the `-p` package — which is now
# rhema-sandbox-server, were the stub not skipped. Declared [[bin]]
# paths are never stubbed either — the awk stubs examples/tests/benches
# only — and fetch needs no bin to exist.
RUN for crate in crates/*/ integrations/*/ tools/*/; do \
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

# The daemon crate pulls only what it serves from: the seam with the
# process machinery (`spawn`) and the local backend — no mock, no HTTP
# client. The stub loop above already wrote its lib.rs; the declared
# [[bin]] path needs no stub for the fetch.
RUN cargo build --release --locked -p rhema-sandbox-server

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
