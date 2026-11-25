# The README demo: a key changing live, sort by size, the command preview, the
# DB switcher and a Pub/Sub notification.
#
# Needs a Redis at redis://demo-redis:6379 with assets/tapes/feed.sh running
# against it, and rrover on the PATH. From the repo root:
#   vhs assets/tapes/demo.tape
#   gifsicle -O3 --lossy=40 --colors 48 assets/demo.gif -o assets/demo.gif
Output assets/demo.gif

Set Shell "bash"
Set FontSize 15
Set Width 1240
Set Height 680
Set Padding 8
Set Framerate 12
Set PlaybackSpeed 1.0
Set TypingSpeed 70ms

Hide
Type "clear" Enter
Show

Type "rrover --url redis://demo-redis:6379 --writable" Sleep 400ms Enter
Sleep 2s

# The open key changes live.
Type "f" Sleep 400ms Type "user:1001" Sleep 500ms Enter Sleep 900ms
Type "j" Sleep 400ms Enter
Sleep 4.5s
Escape Sleep 300ms
Type "l" Sleep 900ms

# Sort by size.
Type "o" Sleep 900ms Type "s" Sleep 1.8s

# Every write shows its command first.
Type "j" Sleep 300ms Type "d" Sleep 2.4s Type "n" Sleep 600ms

# Switch database.
Type "s" Sleep 1.6s Escape Sleep 500ms

# Subscribe, go back to the keys, and get notified.
Type "b" Sleep 700ms Type "a" Sleep 500ms Type "orders" Sleep 400ms Enter Sleep 1s
Escape
Sleep 8s
