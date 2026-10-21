// The deployable entry: the Rust agent DO (built by `worker-build` into
// build/worker/) re-exported beside the JS sandbox shim, plus the
// route that turns /sessions/<id>/... into a DO fetch addressed by the
// session name.
import { CodingAgent } from "../../build/worker/shim.js";
import { SandboxShim } from "./sandbox-shim.mjs";

export { CodingAgent, SandboxShim };

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    const match = url.pathname.match(/^\/sessions\/([^/]+)(\/.*)?$/);
    if (!match) {
      return new Response("route a session as /sessions/<sessionId>/...", {
        status: 404,
      });
    }
    const stub = env.CODING_AGENT.get(
      env.CODING_AGENT.idFromName(decodeURIComponent(match[1])),
    );
    url.pathname = match[2] ?? "/";
    return stub.fetch(new Request(url, request));
  },
};
