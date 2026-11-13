# Release Relay : relais public des mises à jour d'applis Tauri dont le dépôt GitHub est privé
# cargo-chef : les dépendances sont compilées dans une couche à part, gardée en cache tant
# qu'elles ne changent pas (un changement du code ne les recompile pas)
# Images de base à version fixe (mises à jour par Dependabot : .github/dependabot.yml)
FROM lukemathwalker/cargo-chef:0.1.78-rust-1.99.1-bookworm AS chef
WORKDIR /src

FROM chef AS plan
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS build
COPY --from=plan /src/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked --bin release-relay

FROM debian:bookworm-20261027-slim
RUN useradd --system --uid 10001 relay && mkdir /config
COPY --from=build /src/target/release/release-relay /usr/local/bin/release-relay
USER relay
ENV RELAY_CONFIG=/config/relay.toml RELAY_PORT=8080
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s CMD ["release-relay", "health"]
CMD ["release-relay"]
