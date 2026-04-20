FROM ubuntu:24.04

RUN apt-get update && apt-get install -y \
    ca-certificates \
    curl \
    xserver-xorg-core \
    xserver-xorg-video-dummy \
    x11-utils \
    mesa-utils \
    libgl1-mesa-dri \
    libavcodec60 \
    libavutil58 \
    libswscale7 \
    libx264-dev \
    && rm -rf /var/lib/apt/lists/*

COPY target/release/ghostframe-xdaemon /usr/local/bin/ghostframe-xdaemon
COPY target/release/ghostframe-test-pattern /usr/local/bin/ghostframe-test-pattern
COPY tests/containers/test-server/entrypoint.sh /entrypoint.sh
COPY tests/containers/test-server/xorg.conf /etc/X11/xorg.conf
RUN chmod +x /entrypoint.sh

ENTRYPOINT ["/entrypoint.sh"]
