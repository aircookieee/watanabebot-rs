# --- Dashboard frontend ---
FROM node:22-bookworm-slim AS web
WORKDIR /web
COPY package.json ./
RUN npm install --no-audit --no-fund
COPY web ./web
RUN npm run build

# --- Bot binary ---
FROM rust:1.99-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
# Cache dependencies separately from the bot's own sources.
RUN mkdir src && echo "fn main() {}" > src/main.rs && cargo build --release && rm -rf src
COPY src ./src
RUN touch src/main.rs && cargo build --release

# --- Runtime ---
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates tzdata && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=build /src/target/release/watanabebot /usr/local/bin/watanabebot
COPY --from=web /web/dist-web ./dist-web
COPY resources ./resources
COPY discordAniListMap.json ./

ENV APP_ENV=production
EXPOSE 3000
CMD ["watanabebot"]
