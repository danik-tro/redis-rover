# Six Redis nodes (3 masters, 3 replicas) in one container, on the container's
# loopback, with the same ports published on the host. Every node announces
# 127.0.0.1:<port>, which works inside the container and from the host.
FROM redis:7-alpine
COPY cluster-entrypoint.sh /usr/local/bin/cluster-entrypoint.sh
RUN chmod +x /usr/local/bin/cluster-entrypoint.sh
EXPOSE 7100-7105
ENTRYPOINT ["/usr/local/bin/cluster-entrypoint.sh"]
