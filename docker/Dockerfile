ARG BASE_IMAGE
FROM ${BASE_IMAGE}
LABEL org.opencontainers.image.source=https://github.com/greenroom-robotics/pixi
ARG TARGETARCH
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --chmod=0755 pixi-${TARGETARCH} /usr/local/bin/pixi
