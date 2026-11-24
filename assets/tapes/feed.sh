#!/bin/sh
# Data for the demo: a seeded keyspace, one hash that keeps changing, and a
# Pub/Sub message now and then. REDIS_HOST is the Redis to write to.
set -eu
cli() { redis-cli -h "${REDIS_HOST:-localhost}" "$@" >/dev/null; }

cli FLUSHALL
cli HSET user:1001 name Ada plan pro visits 1
for i in 2 3 4 5 6 7 8; do cli HSET "user:100$i" name "user $i" plan free; done
for i in 1 2 3 4 5 6; do cli SET "session:$i" "token-$i" EX "$((i * 900))"; done
cli SET cache:report "$(head -c 40000 /dev/zero | tr '\0' x)"
cli SET cache:home "$(head -c 6000 /dev/zero | tr '\0' y)"
cli RPUSH queue:mail a b c d e f g
cli ZADD board:top 42 ada 37 lin 12 grace
cli SADD tags:new redis rust tui
cli -n 1 SET other:db:key value
cli -n 2 SET archive:2025 value

n=1
while :; do
    n=$((n + 1))
    cli HSET user:1001 visits "$n"
    if [ $((n % 6)) -eq 0 ]; then
        cli PUBLISH orders "order $n paid"
    fi
    sleep 1
done
