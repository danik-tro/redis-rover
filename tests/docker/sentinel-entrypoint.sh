#!/bin/sh
# Starts every process daemonized under a loop that stays alive when one is
# killed, so a test can kill the master without stopping the container.
set -eu

MASTER=7200
REPLICAS="7201 7202"
SENTINELS="7210 7211 7212"

start_redis() {
    port="$1"
    shift
    mkdir -p "/data/$port"
    redis-server --port "$port" --bind 0.0.0.0 --protected-mode no \
        --dir "/data/$port" --appendonly no --save '' \
        --replica-announce-ip 127.0.0.1 --replica-announce-port "$port" \
        --daemonize yes --logfile "/data/$port/redis.log" \
        --pidfile "/data/$port/redis.pid" "$@"
    until redis-cli -p "$port" ping >/dev/null 2>&1; do sleep 0.1; done
}

start_redis "$MASTER"
for port in $REPLICAS; do
    start_redis "$port" --replicaof 127.0.0.1 "$MASTER"
done
for port in $REPLICAS; do
    until redis-cli -p "$port" info replication | grep -q 'master_link_status:up'; do
        sleep 0.1
    done
done

for port in $SENTINELS; do
    mkdir -p "/data/$port"
    cat > "/data/$port/sentinel.conf" <<CONF
port $port
bind 0.0.0.0
protected-mode no
dir /data/$port
daemonize yes
logfile /data/$port/sentinel.log
pidfile /data/$port/sentinel.pid
sentinel announce-ip 127.0.0.1
sentinel announce-port $port
sentinel monitor mymaster 127.0.0.1 $MASTER 2
sentinel down-after-milliseconds mymaster 1000
sentinel failover-timeout mymaster 5000
sentinel parallel-syncs mymaster 1
CONF
    redis-sentinel "/data/$port/sentinel.conf"
done

# Ready once every Sentinel knows both replicas and the other two Sentinels.
for port in $SENTINELS; do
    until [ "$(redis-cli -p "$port" sentinel master mymaster | grep -A1 -x 'num-slaves' | tail -1)" = 2 ] &&
        [ "$(redis-cli -p "$port" sentinel master mymaster | grep -A1 -x 'num-other-sentinels' | tail -1)" = 2 ]; do
        sleep 0.1
    done
done
touch /data/ready
echo "sentinel ready"

trap 'exit 0' TERM INT
while :; do sleep 1 & wait $!; done
