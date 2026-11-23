# Keybindings

<!-- Generated from .config/config.json5. Do not edit: run
`UPDATE_DOCS=1 cargo test keybindings_reference`. -->

The default keys. Rebind any of them in your config file; the `?` help and
the hint bar always show the keys in force. `g o` is a chord: `g`, then `o`.

## Everywhere

| Keys | Action |
| --- | --- |
| `Ctrl-c` | Quit |

## Key list and values

| Keys | Action |
| --- | --- |
| `j` | Scroll down |
| `k` | Scroll up |
| `n` | Next page (keys or open value) |
| `p` | Previous page (keys or open value) |
| `r` | Refresh keyspace |
| `f` | Set filter pattern |
| `l` | Delete filter pattern |
| `o` | Sort by key / type / TTL / size |
| `Shift-T` | Show only some types |
| `v` | Tree view (←/→ fold, Enter opens) |
| `Shift-A` | Load all keys (Esc stops) |
| `g o` | Go to the open key |
| `Enter` | Enter value / JSON field / confirm |
| `Tab` | Stream: entries / groups |
| `a` | Add new key |
| `i` | Add item (in collection) |
| `e` | Edit value / item (drill in first) |
| `Shift-E` | Edit value / item in $EDITOR |
| `d` | Delete key / item (in value) |
| `t` | Set TTL |
| `F2` | Rename key / field / member |
| `c` | Copy key (Tab: target DB) |
| `Space` | Mark / unmark key (d deletes marked) |
| `Shift-M` | Mark all keys on the page |
| `Shift-U` | Clear marks |
| `y` | Copy value / item |
| `Shift-Y` | Copy key name |
| `w` | Wrap text value on / off |
| `Left` | Pan text left (wrap off) |
| `Right` | Pan text right (wrap off) |
| `Ctrl-Left` | Narrow the key pane |
| `Ctrl-Right` | Widen the key pane |
| `s` | Switch database |
| `m` | Open metrics dashboard |
| `b` | Open Pub/Sub |
| `Esc` | Close popup / overlay |
| `Shift-R` | Toggle read-only |
| `?` | Toggle this help |
| `g m` | Go to MONITOR |
| `g s` | Go to Slowlog |
| `g p` | Go to Pub/Sub |
| `g d` | Go to metrics dashboard |

## Dashboard

| Keys | Action |
| --- | --- |
| `m`, `Esc` | Back to keyspace |
| `Space` | Pause / resume view |
| `r` | Reset history |
| `l`, `Right` | Next chart |
| `h`, `Left` | Previous chart |
| `Tab` | Next node (one server: next chart) |
| `Shift-Backtab` | Previous node (one server: previous chart) |
| `a` | Toggle all charts |
| `b` | Open Pub/Sub |
| `Shift-R` | Toggle read-only |
| `?` | Toggle this help |
| `g m` | Go to MONITOR |
| `g s` | Go to Slowlog |
| `g p` | Go to Pub/Sub |
| `g d` | Go to metrics dashboard |

## Pub/Sub

| Keys | Action |
| --- | --- |
| `b`, `Esc` | Back (Esc closes the detail first) |
| `j`, `Down` | Down (list, log or detail) |
| `k`, `Up` | Up (list, log or detail; pauses the log) |
| `Tab` | Switch focus: list / log |
| `Space` | Pause / follow the log |
| `Enter` | Subscribe to server channel / open message |
| `y` | Copy message payload |
| `Shift-Y` | Copy message channel |
| `w` | Wrap message detail on / off |
| `Left` | Pan detail left (wrap off) |
| `Right` | Pan detail right (wrap off) |
| `a` | Add subscription (Tab: kind) |
| `d` | Unsubscribe and drop messages |
| `c` | Clear messages and stats |
| `Shift-P` | Publish (Ctrl-t: SPUBLISH) |
| `Shift-N` | Toggle notifications (also un-mutes) |
| `r` | Refresh server channels |
| `Shift-R` | Toggle read-only |
| `?` | Toggle this help |
| `g m` | Go to MONITOR |
| `g s` | Go to Slowlog |
| `g p` | Go to Pub/Sub |
| `g d` | Go to metrics dashboard |

## MONITOR

| Keys | Action |
| --- | --- |
| `Esc` | Back (Esc closes the detail first) |
| `t` | Start / stop MONITOR |
| `/` | Filter by command or key glob |
| `Space` | Pause / follow the log |
| `c` | Clear the log |
| `Enter` | Open the selected line |
| `Tab` | Next cluster node |
| `Shift-Backtab` | Previous cluster node |
| `j`, `Down` | Down (log or detail) |
| `k`, `Up` | Up (log or detail; pauses the log) |
| `w` | Wrap the detail on / off |
| `Left` | Pan detail left (wrap off) |
| `Right` | Pan detail right (wrap off) |
| `Shift-R` | Toggle read-only |
| `?` | Toggle this help |
| `g m` | Go to MONITOR |
| `g s` | Go to Slowlog |
| `g p` | Go to Pub/Sub |
| `g d` | Go to metrics dashboard |

## Slowlog

| Keys | Action |
| --- | --- |
| `Esc` | Back (Esc closes the detail first) |
| `Enter` | Open the entry's full arguments |
| `o` | Sort: newest / slowest first |
| `r` | Load it again |
| `Shift-R` | SLOWLOG RESET (shows a preview) |
| `Tab` | Next cluster node |
| `Shift-Backtab` | Previous cluster node |
| `j`, `Down` | Down (table or detail) |
| `k`, `Up` | Up (table or detail) |
| `w` | Wrap the detail on / off |
| `Left` | Pan detail left (wrap off) |
| `Right` | Pan detail right (wrap off) |
| `?` | Toggle this help |
| `g m` | Go to MONITOR |
| `g s` | Go to Slowlog |
| `g p` | Go to Pub/Sub |
| `g d` | Go to metrics dashboard |

## Text inputs

Fixed, not rebindable.

| Keys | Action |
| --- | --- |
| `Ctrl-U / Ctrl-R` | Undo / redo in a text input |
