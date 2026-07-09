FROM rust:latest AS build-stage
WORKDIR /usr/src/app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo install --path .

FROM rust:slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates iputils-ping \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build-stage /usr/local/cargo/bin/uptime_exporter /usr/local/bin/uptime_exporter
EXPOSE 9200
ENTRYPOINT ["uptime_exporter"]
