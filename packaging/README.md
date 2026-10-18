# Packaging

1. Install the binaries: `vuoxi` to `~/.local/bin` (or `/usr/local/bin`; adjust
   `ExecStart` in `vuoxi.service`) and `vuoxi-watcher` to `/usr/local/bin`.
2. Create the device key and config, then pair and add folders:

   ```
   vuoxi init
   vuoxi pair laptop <peer-id> '[2001:db8::1]:41641'
   vuoxi folder add docs ~/docs --peer laptop
   ```

   Config is `~/.config/vuoxi/config.toml`, the key sits beside it (mode 0600).
3. Socket directory for the watchers, owned by the user running the daemon
   (replace `USER`). Put this in `/etc/tmpfiles.d/vuoxi.conf`:

   ```
   d /run/vuoxi 0700 USER USER -
   ```

   Then `systemd-tmpfiles --create`.
4. Daemon (user unit):

   ```
   mkdir -p ~/.config/systemd/user
   cp packaging/vuoxi.service ~/.config/systemd/user/
   systemctl --user daemon-reload
   systemctl --user enable --now vuoxi.service
   ```

5. One watcher per filesystem that holds a synced folder (system unit, root):

   ```
   sudo cp packaging/vuoxi-watcher@.service /etc/systemd/system/
   sudo systemctl daemon-reload
   sudo systemctl enable --now "vuoxi-watcher@$(systemd-escape --path /home).service"
   ```

   The socket is `/run/vuoxi/<escaped-mount>.sock`; the daemon listens there and
   the watcher connects, retrying every 5 s until the daemon is up. Point the
   daemon at it in `~/.config/vuoxi/config.toml`:

   ```toml
   watcher_socket = "/run/vuoxi/home.sock"
   ```

   Without a watcher (no root on the machine), leave `watcher_socket` unset:
   `vuoxi run` then reconciles every `VUOXI_POLL_SECS` seconds (default 60),
   comparing metadata and hashing only what changed.

6. Pair the devices. On each: `vuoxi init`, then exchange ids:

   ```
   vuoxi pair laptop <its id> [2001:db8::2]:41641
   vuoxi folder add notes ~/notes --peer laptop
   ```

`systemd-analyze verify packaging/*.service` checks the units.

## A relay

A relay is an always-on machine that stores your folders **encrypted** and
serves them to your devices, so two devices never need to be online at the same
time. It holds no folder keys and cannot read names or contents.

On the relay (a small container is enough):

```
vuoxi init                                   # prints the relay's id
vuoxi pair laptop <laptop id> [2001:db8::2]:41641
vuoxi pair desktop <desktop id> [2001:db8::1]:41641
# config.toml: relay_dir = "/var/lib/vuoxi/relay"   (no [[folder]] entries)
vuoxi run
```

On every device:

```
vuoxi pair relay <relay id> [2001:db8::10]:41641 --relay
# add "relay" to the folder's peers; every device must use the same folder key:
vuoxi folder key notes          # on the device that created the folder
# then `key = "<that hex>"` in the folder's [[folder]] block on the others
```

Firewall the relay so only your devices reach UDP 41641. It keeps your current
files plus replaced data for `trash_days` (30 by default); give its storage a
quota anyway.

## Dashboard

`vuoxi run` serves it on `127.0.0.1:41642`. Build `ui/` (`pnpm build`), copy
`ui/dist/` to `~/.local/share/vuoxi/ui/`, then open the link `vuoxi ui` prints.
