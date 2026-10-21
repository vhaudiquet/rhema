// The sandbox shim: a small JS Durable Object that turns plain JSON
// over HTTP into Durable Object Container calls. The container runtime
// (`ctx.container`) is a JS-only API — workers-rs cannot reach it — so
// the Rust agent DO speaks this surface instead of fighting JS RPC.
//
// One shim instance per session, named after the session like the
// agent DO, so the container's lifetime hangs off the same name the
// checkpoints do. `SANDBOX_TOKEN` guards the HTTP surface: the agent
// DO sends it in `x-rhema-token`, and nothing else should answer here.
//
// Contract (all routes JSON):
//   POST /exec  { command, cwd?, timeout_secs }
//            -> { exit_code, stdout, stderr, timed_out }
//   POST /put   { path, content_base64 } -> { error: null | message }
//   POST /get   { path } -> { content_base64 } | { error: message }
//   POST /seed  { git } | { tarball_base64 } -> { error: null | message }
//
// Output streams are tail-capped before they cross the wire, so a
// runaway command cannot flood the DO; the tool applies its display
// caps again on top.

const MAX_STREAM_BYTES = 50 * 1024;
// The put/get/seed routes ride command arguments, which are bounded;
// keep the payloads comfortably inside that.
const MAX_FILE_BYTES = 512 * 1024;
const IMAGE = "cloudflare/debian-trixie";
// Keep the container warm for a quarter hour after the last command —
// the idle sleep the pricing caveat is about (see README.md).
const INACTIVITY_TIMEOUT_MS = 15 * 60 * 1000;

/// Shell-quotes one argument, single-quote style.
function shquote(value) {
  return `'${String(value).replaceAll(`'`, `'\\''`)}'`;
}

function cap(text) {
  return text.length <= MAX_STREAM_BYTES ? text : text.slice(text.length - MAX_STREAM_BYTES);
}

function clampTimeout(secs) {
  const n = Number(secs);
  if (!Number.isFinite(n)) return 120;
  return Math.min(Math.max(Math.round(n), 1), 600);
}

export class SandboxShim {
  constructor(ctx, env) {
    this.ctx = ctx;
    this.env = env;
  }

  async ensureRunning() {
    const container = this.ctx.container;
    if (!container) {
      throw new Error("the container binding is not configured");
    }
    if (!container.running) {
      container.start({
        image: IMAGE,
        entrypoint: ["sleep", "infinity"],
        enableInternet: true,
      });
      await container.setInactivityTimeout(INACTIVITY_TIMEOUT_MS);
    }
  }

  /// Runs one command with a wall-clock budget; `timeout` kills the
  /// command and every process it started.
  async exec(command, timeoutSecs) {
    await this.ensureRunning();
    const argv = [
      "timeout",
      "--kill-after=5",
      String(timeoutSecs),
      "bash",
      "-c",
      command,
    ];
    const process = await this.ctx.container.exec(argv);
    const output = await process.output();
    const decoder = new TextDecoder();
    return {
      exit_code: output.exitCode,
      stdout: cap(decoder.decode(output.stdout)),
      stderr: cap(decoder.decode(output.stderr)),
      // `timeout` exits 124 when it kills; with --kill-after the kill
      // lands as SIGKILL, which the shell reports as 137.
      timed_out: output.exitCode === 124 || output.exitCode === 137,
    };
  }

  async fetch(request) {
    const token = this.env.SANDBOX_TOKEN;
    if (token && request.headers.get("x-rhema-token") !== token) {
      return Response.json({ error: "forbidden" }, { status: 403 });
    }
    const route = new URL(request.url).pathname;
    let body = {};
    try {
      body = await request.json();
    } catch {
      // Routes that need a body fail on their own below.
    }
    try {
      switch (route) {
        case "/healthz":
          return Response.json({ ok: true });
        case "/exec": {
          const cwd =
            typeof body.cwd === "string" && body.cwd
              ? `cd ${shquote(body.cwd)} && `
              : "";
          const result = await this.exec(
            `${cwd}{ ${body.command ?? ":"} ; }`,
            clampTimeout(body.timeout_secs),
          );
          return Response.json(result);
        }
        case "/put": {
          const path = String(body.path ?? "");
          const b64 = String(body.content_base64 ?? "");
          if (b64.length > (MAX_FILE_BYTES * 4) / 3 + 4) {
            return Response.json({ error: "file too large" }, { status: 413 });
          }
          const result = await this.exec(
            `mkdir -p "$(dirname ${shquote(path)})" && ` +
              `printf %s ${shquote(b64)} | base64 -d > ${shquote(path)}`,
            60,
          );
          if (result.exit_code !== 0) {
            return Response.json(
              { error: result.stderr || "write failed" },
              { status: 500 },
            );
          }
          return Response.json({ error: null });
        }
        case "/get": {
          const path = String(body.path ?? "");
          const result = await this.exec(
            `base64 ${shquote(path)} 2>/dev/null | head -c ${MAX_FILE_BYTES * 2}`,
            60,
          );
          if (result.exit_code !== 0) {
            return Response.json({ error: "no such file" }, { status: 404 });
          }
          return Response.json({
            content_base64: result.stdout.replace(/\s+/g, ""),
            error: null,
          });
        }
        case "/seed": {
          if (body.git) {
            const result = await this.exec(
              `git clone ${shquote(String(body.git))} .`,
              300,
            );
            if (result.exit_code !== 0) {
              return Response.json(
                { error: result.stderr || "clone failed" },
                { status: 500 },
              );
            }
          } else if (body.tarball_base64) {
            const result = await this.exec(
              `printf %s ${shquote(String(body.tarball_base64))} | base64 -d | tar -xzf -`,
              300,
            );
            if (result.exit_code !== 0) {
              return Response.json(
                { error: result.stderr || "unpack failed" },
                { status: 500 },
              );
            }
          }
          return Response.json({ error: null });
        }
        default:
          return Response.json({ error: "not found" }, { status: 404 });
      }
    } catch (err) {
      return Response.json({ error: String(err) }, { status: 500 });
    }
  }
}
