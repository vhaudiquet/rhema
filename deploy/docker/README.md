# Docker images

Every Dockerfile here builds from the repository root as its context:

```sh
docker build -f deploy/docker/<name>.Dockerfile -t <name> .
```

## rhema-sandboxd

The sandbox wire protocol served from a host: one root directory per
session under a fixed base, `bash -c` exec under a `timeout` budget,
files and seeds on the fs, the token checked on every route
(`specs/rhema-sandbox.md` §3).

Build and run:

```sh
docker build -f deploy/docker/sandboxd.Dockerfile -t rhema-sandboxd .

docker run -d --name sandboxd \
  -p 7600:7600 \
  -v sandboxd-base:/var/lib/rhema-sandboxd \
  rhema-sandboxd --port 7600 --token "$(openssl rand -hex 32)"
```

- `--token` has no environment default: the runner passes the shared
  secret on the command line; clients send it in `x-rhema-token`.
- `--port` must be passed — the binary's default is `0` (OS-picked),
  which a published mapping cannot follow. The image binds `0.0.0.0`
  (the binary's own default is loopback; overriding the host means
  overriding the entrypoint).
- The base (`-v`) holds one directory per session name; mount a volume
  or bind mount there, or every session dies with the container.

A client reaches it with `HttpSandbox` (feature `client`): the base URL
is `http://<host>:<port>`, and the session name is the first path
segment of every route — `POST {base}/{session}/exec|/put|/get|/seed`,
plus `GET /healthz` at the root, token required there too.
