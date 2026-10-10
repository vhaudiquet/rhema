# rhema-claw-ui: the web UI's static bundle behind nginx.
# Build from the repo root (the context is the repo):
#   docker build -f deploy/docker/ui.Dockerfile -t rhema-claw-ui .

# The build: the Nuxt app generates a fully static bundle (ssr is off —
# the UI is a client of the claw's API, nothing server-side to run).
FROM node:22-alpine AS build
WORKDIR /web
COPY web/claw/package.json web/claw/package-lock.json ./
# The lock file pins the tree; ci is the reproducible install.
RUN npm ci
COPY web/claw ./
RUN npm run build

# The runtime: nginx serves the bundle with an SPA fallback — the app is
# one page, but a browser hitting a stale deep link must land on it, not
# on a 404. Index is served no-cache (a new deploy must not be masked by
# the browser), the hashed _nuxt assets are forever-cacheable.
FROM nginx:alpine
COPY <<'NGINX' /etc/nginx/conf.d/default.conf
server {
    listen 80;
    root /usr/share/nginx/html;
    index index.html;

    location /_nuxt/ {
        add_header Cache-Control "public, max-age=31536000, immutable";
        try_files $uri =404;
    }
    location / {
        add_header Cache-Control "no-cache";
        try_files $uri $uri/ /index.html;
    }
}
NGINX
COPY --from=build /web/.output/public /usr/share/nginx/html

# The UI is stateless static files: any number of replicas, no PVC, and
# the claw's single-node invariant does not apply here.
EXPOSE 80
