# Minimal runtime image wrapping the CI-released agent-gateway binary.
# Binary statically links SQLite + sqlite-vec; only libc is dynamic (glibc>=2.38) -> ubuntu:24.04.
FROM ubuntu:24.04
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY gateway /usr/local/bin/gateway
RUN chmod 755 /usr/local/bin/gateway
ENV GATEWAY_HOST=0.0.0.0 GATEWAY_PORT=7913 DATABASE_PATH=/data/agent-gateway.db
EXPOSE 7913
ENTRYPOINT ["/usr/local/bin/gateway"]
