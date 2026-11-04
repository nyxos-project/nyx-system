# Nyx System

NyxOS's system services: the layer Nyx Settings, Welcome, Store, the `nyx` command
and recovery tools share for changing the system, the configuration layers, and the
theme service.

| Part | What it is |
| --- | --- |
| `nyx-system-manager` | Root D-Bus service (`io.github.nyxos_project.System`), started on demand: validated, polkit-authorized system changes through the NixOS configuration, updates, generations and rollback |
| `nyx-system` | Its command line, and the configuration layers' |
| `nyx-config` | The configuration layers as a library |
| `nyx-system-proto` | The settings, changes and D-Bus interface, for clients |
| `nyx-themed` | User service: the chosen Nyx theme on Hyprland, Foot, GTK and the wallpaper, live |

## Configuration layers

1. Distribution defaults, `/etc/nyxos/defaults/DOMAIN.toml`, from the NixOS configuration.
2. System overrides, `/etc/nyxos/overrides/`, written by the System Manager.
3. User settings, `~/.config/nyx/DOMAIN.toml`, written by the user's apps.

Later layers win key by key. Updates replace only the first layer.

Ordinary desktop preferences are user settings and change live. System settings
(`/etc/nyxos/overrides/system.json`: host name, time zone, locale, keyboard, system
packages, the services NyxOS manages, the system, boot and login themes) are read by the
NyxOS flake as its `nyxos-overrides` input. The manager builds a change before switching
to it, and puts the previous settings back if the build or the switch fails. It keeps
the settings each generation was built from, so a rollback brings them back too.

## The System Manager

Each method checks its caller with polkit, one action per kind of change
(`io.github.nyxos_project.system.{settings,packages,services,defaults,update,rollback}`),
and writes an audit line to the journal. Clients send typed changes
(`nyx_system_proto::Change`), never commands. One change runs at a time.

```sh
nyx-system settings
nyx-system set time-zone America/Chicago
nyx-system install gimp
nyx-system service printing on
nyx-system update
nyx-system generations
nyx-system rollback
nyx-system default set appearance theme nord   # every new user's theme
nyx-system config show appearance              # merged, with each value's layer
```

## Themes

The system theme (`nyx-system set theme`) is every user's default and the boot and login
screens', unless those are set on their own (`boot-theme`, `login-theme`). Each user's
desktop theme is the one their Nyx apps follow (`~/.config/nyx/theme.toml`), carried to
the rest of the desktop by `nyx-themed`; their lock-screen theme is `appearance.toml`'s
`lock-theme`.

## Build

```sh
cargo build --release
cargo test --workspace
```

## License

MIT
