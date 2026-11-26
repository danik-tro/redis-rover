#!/bin/sh
# Data for the demo: a seeded keyspace of every type, one hash that keeps
# changing, Pub/Sub messages and some traffic for MONITOR and the dashboard.
# REDIS_HOST is the Redis to write to.
set -eu
cli() { redis-cli -h "${REDIS_HOST:-localhost}" "$@" >/dev/null; }

cli FLUSHALL
cli HSET user:1001 name Ada email ada@example.com plan pro visits 1
for i in 2 3 4 5 6 7 8; do
    cli HSET "user:100$i" name "user $i" email "user$i@example.com" plan free visits "$i"
done
for i in 1 2 3 4 5 6; do cli SET "session:$i" "token-$i" EX "$((i * 900))"; done
cli SET app:config:feature_flags '{"dark_mode":true,"beta":{"search":true,"export":false},"limits":{"rpm":600,"burst":40},"regions":["eu-west-1","us-east-1"]}'
cli SET cache:report "$(head -c 40000 /dev/zero | tr '\0' x)"
cli SET cache:home "$(head -c 6000 /dev/zero | tr '\0' y)"
cli RPUSH queue:mail welcome:1001 reset:1004 invoice:1002 digest:1007 welcome:1008
cli ZADD board:top 4200 ada 3700 lin 3100 grace 2600 alan 1200 barbara
cli SADD tags:popular redis rust tui terminal cli
cli XADD events:orders '*' order 501 status paid amount 42.50
cli XADD events:orders '*' order 502 status shipped amount 18.00
cli XADD events:orders '*' order 503 status paid amount 99.90
cli -n 1 SET other:db:key value
cli -n 2 SET archive:2025 value

n=1
while :; do
    n=$((n + 1))
    cli HSET user:1001 visits "$n"
    cli GET "session:$(((n % 6) + 1))"
    cli LRANGE queue:mail 0 -1
    cli ZINCRBY board:top 7 lin
    if [ $((n % 3)) -eq 0 ]; then
        cli PUBLISH orders "{\"order\":$((500 + n)),\"status\":\"paid\"}"
    fi
    sleep 1
done
