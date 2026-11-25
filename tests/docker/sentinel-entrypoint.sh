#!/bin/sh
# Starts every process daemonized under a loop that stays alive when one is
# killed, so a test can kill the master without stopping the container.
set -eu

MASTER=7200
REPLICAS="7201 7202"
SENTINELS="7210 7211 7212"
# Set: every data node needs it (requirepass + masterauth), the Sentinels
# monitor with it (auth-pass) and stay open themselves.
PASSWORD="${RROVER_FIXTURE_PASSWORD:-}"
AUTH_PASS=""
if [ -n "$PASSWORD" ]; then
    AUTH_PASS="sentinel auth-pass mymaster $PASSWORD"
fi

node_cli() {
    if [ -n "$PASSWORD" ]; then
        REDISCLI_AUTH="$PASSWORD" redis-cli "$@"
    else
        redis-cli "$@"
    fi
}

start_redis() {
    port="$1"
    shift
    if [ -n "$PASSWORD" ]; then
        set -- "$@" --requirepass "$PASSWORD" --masterauth "$PASSWORD"
    fi
    mkdir -p "/data/$port"
    redis-server --port "$port" --bind 0.0.0.0 --protected-mode no \
        --dir "/data/$port" --appendonly no --save '' \
        --replica-announce-ip 127.0.0.1 --replica-announce-port "$port" \
        --daemonize yes --logfile "/data/$port/redis.log" \
        --pidfile "/data/$port/redis.pid" "$@"
    until [ "$(node_cli -p "$port" ping 2>/dev/null)" = PONG ]; do sleep 0.1; done
}

start_redis "$MASTER"
for port in $REPLICAS; do
    start_redis "$port" --replicaof 127.0.0.1 "$MASTER"
done
for port in $REPLICAS; do
    until node_cli -p "$port" info replication | grep -q 'master_link_status:up'; do
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
$AUTH_PASS
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
