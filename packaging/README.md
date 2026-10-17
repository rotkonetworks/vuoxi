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
