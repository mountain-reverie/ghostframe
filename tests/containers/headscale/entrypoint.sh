#!/bin/sh
# SPDX-License-Identifier: MIT OR Apache-2.0
# If HS_SERVER_URL is set, overwrite the server_url in the config.
if [ -n "${HS_SERVER_URL:-}" ]; then
    sed -i "s|^server_url:.*|server_url: ${HS_SERVER_URL}|" /etc/headscale/config.yaml
fi
exec headscale serve
