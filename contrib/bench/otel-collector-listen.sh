#!/bin/sh
# Let rootless podman containers (Harbor benchmark trials) reach the host's
# OTLP collector. Run as root on moltar.
#
# The collector runs with host networking and listens on 127.0.0.1 only. A
# rootless container reaches the host through host.containers.internal, which
# does not arrive at a loopback-only listener. This changes the gRPC receiver
# to listen on all addresses, so the LAN can reach port 4317 too.
#
# Containers then use OTEL_EXPORTER_OTLP_ENDPOINT=http://host.containers.internal:4317
set -eu

config=/etc/otelcol/config.yaml

cp -a "$config" "$config.bak.$(date +%Y%m%d-%H%M%S)"
sed -i 's|endpoint: 127.0.0.1:4317|endpoint: 0.0.0.0:4317|' "$config"
grep -n 'endpoint: 0.0.0.0:4317' "$config"

systemctl restart otel-collector.service
sleep 2
ss -ltn | grep ':4317'
