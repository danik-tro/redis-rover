# The README demo, in the default dark theme against a local Redis: the tree,
# a key changing live, a JSON value, sort by size, adding a key (with the
# command preview), Pub/Sub, MONITOR and the dashboard.
#
# Needs a Redis on localhost:6379 (so the profile is the default LOCAL one)
# with assets/tapes/feed.sh running against it, and rrover on the PATH.
# From the repo root:
#   vhs assets/tapes/demo.tape
#   gifsicle -O3 --lossy=60 --colors 48 assets/demo.gif -o assets/demo.gif
Output assets/demo.gif

Set Shell "bash"
Set FontSize 14
Set Width 1300
Set Height 740
Set Padding 10
Set Framerate 10
Set PlaybackSpeed 1.0
Set TypingSpeed 60ms
# The terminal in the colours of rrover's bundled dark palette.
Set Theme { "name": "rrover-dark", "background": "#282936", "foreground": "#e9e9f4", "cursor": "#e9e9f4", "selection": "#4d4f68", "black": "#282936", "red": "#ea51b2", "green": "#00f769", "yellow": "#ebff87", "blue": "#62d6e8", "magenta": "#b45bcf", "cyan": "#a1efe4", "white": "#e9e9f4", "brightBlack": "#626483", "brightRed": "#ea51b2", "brightGreen": "#00f769", "brightYellow": "#ebff87", "brightBlue": "#62d6e8", "brightMagenta": "#b45bcf", "brightCyan": "#a1efe4", "brightWhite": "#f7f7fb" }
Env COLORTERM "truecolor"
# A fresh data dir, so no saved session (filter, subscriptions) is restored.
Env REDIS_ROVER_DATA "/tmp/rrover-demo"

Hide
Type "rm -rf /tmp/rrover-demo && clear" Enter
Show

# No flags: redis://localhost:6379, the LOCAL environment, writable.
Type "rrover" Sleep 400ms Enter
Sleep 2.5s

# Keys as a tree of prefixes.
Type "v" Sleep 1.8s Type "v" Sleep 500ms

# Filter, open a hash: it changes live.
Type "f" Sleep 400ms Type "user:1001" Sleep 400ms Enter Sleep 800ms
Type "j" Sleep 300ms Enter
Sleep 4s
Escape Sleep 300ms Type "l" Sleep 600ms

# A JSON string, pretty-printed.
Type "f" Sleep 400ms Type "app:*" Sleep 400ms Enter Sleep 800ms
Type "j" Sleep 300ms Enter
Sleep 2.8s
Escape Sleep 300ms Type "l" Sleep 600ms

# Sort by size.
Type "o" Sleep 900ms Type "s" Sleep 1.5s

# Add a hash. Every write shows its command first.
Type "a" Sleep 900ms Type "jjj" Sleep 400ms Enter Sleep 500ms
Type "cart:1001" Sleep 400ms Enter Sleep 500ms
Type "sku" Tab Type "kbd-42" Sleep 300ms Enter
Type "qty" Tab Type "2" Sleep 300ms Enter Sleep 500ms
Ctrl+S Sleep 2.2s
Type "y" Sleep 2.2s

# Subscribe to a channel.
Type "b" Sleep 800ms Type "a" Sleep 500ms Type "orders" Sleep 400ms Enter
Sleep 5s

# MONITOR asks before it starts.
Type "gm" Sleep 1.5s Type "y" Sleep 4.5s

# The dashboard.
Type "gd" Sleep 4s
