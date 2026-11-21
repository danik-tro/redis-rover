#!/bin/sh
# Starts the nodes under a small supervisor loop that stays alive when one is
# killed, so a test can kill a single master without stopping the container.
# A killed node stays down until `restart-node <port>`.
set -eu

PORTS="${CLUSTER_PORTS:-7100 7101 7102 7103 7104 7105}"

start_node() {
    port="$1"
    mkdir -p "/data/$port"
    redis-server \
        --port "$port" \
        --bind 0.0.0.0 \
        --protected-mode no \
        --dir "/data/$port" \
        --cluster-enabled yes \
        --cluster-config-file nodes.conf \
        --cluster-node-timeout 2000 \
        --cluster-announce-ip 127.0.0.1 \
        --appendonly no \
        --save '' \
        --daemonize yes \
        --logfile "/data/$port/redis.log" \
        --pidfile "/data/$port/redis.pid"
}

cat > /usr/local/bin/restart-node <<SCRIPT
#!/bin/sh
exec redis-server --port "\$1" --bind 0.0.0.0 --protected-mode no \
    --dir "/data/\$1" --cluster-enabled yes --cluster-config-file nodes.conf \
    --cluster-node-timeout 2000 --cluster-announce-ip 127.0.0.1 \
    --appendonly no --save '' --daemonize yes \
    --logfile "/data/\$1/redis.log" --pidfile "/data/\$1/redis.pid"
SCRIPT
chmod +x /usr/local/bin/restart-node

nodes=""
for port in $PORTS; do
    start_node "$port"
    until redis-cli -p "$port" ping >/dev/null 2>&1; do sleep 0.1; done
    nodes="$nodes 127.0.0.1:$port"
done

# shellcheck disable=SC2086
redis-cli --cluster create $nodes --cluster-replicas 1 --cluster-yes >/data/create.log

until redis-cli -p "${PORTS%% *}" cluster info | grep -q 'cluster_state:ok'; do sleep 0.1; done

# A replica that never finished its first sync refuses to fail over, so the
# cluster isn't ready until every replica is linked to its master.
total=$(echo "$PORTS" | wc -w)
while :; do
    linked=0
    for port in $PORTS; do
        if redis-cli -p "$port" info replication | grep -q 'master_link_status:up'; then
            linked=$((linked + 1))
        fi
    done
    [ "$linked" -ge $((total / 2)) ] && break
    sleep 0.1
done
touch /data/ready
echo "cluster ready"

# PID 1 outlives any single node.
trap 'exit 0' TERM INT
while :; do sleep 1 & wait $!; done
