# One master, two replicas and three Sentinels in one container, on the
# container's loopback, with the same ports published on the host. Everything
# announces 127.0.0.1:<port>, which works inside the container and from the host.
FROM redis:7-alpine
COPY sentinel-entrypoint.sh /usr/local/bin/sentinel-entrypoint.sh
RUN chmod +x /usr/local/bin/sentinel-entrypoint.sh
EXPOSE 7200-7202 7210-7212
ENTRYPOINT ["/usr/local/bin/sentinel-entrypoint.sh"]
