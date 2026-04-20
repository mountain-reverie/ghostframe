FROM busybox:latest AS busybox

FROM headscale/headscale:0.28.0

# Copy busybox shell so we can run the entrypoint script.
COPY --from=busybox /bin/sh /bin/sh
COPY --from=busybox /bin/sed /bin/sed

COPY config/config.yaml /etc/headscale/config.yaml
COPY entrypoint.sh /entrypoint.sh

ENTRYPOINT ["/bin/sh", "/entrypoint.sh"]
