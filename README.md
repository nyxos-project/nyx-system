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
| `nyx-catalog` | The software catalog (`crates/nyx-catalog/catalog.toml`), and the changes that install from it |
| `nyx-themed` | User service: the chosen Nyx theme on Hyprland, Foot, GTK and the wallpaper, live |

## Configuration layers

1. Distribution defaults, `/etc/nyxos/defaults/DOMAIN.toml`, from the NixOS configuration.
2. System overrides, `/etc/nyxos/overrides/`, written by the System Manager.
3. User settings, `~/.config/nyx/DOMAIN.toml`, written by the user's apps.

Later layers win key by key. Updates replace only the first layer.

Ordinary desktop preferences are user settings and change live. System settings
(`/etc/nyxos/overrides/system.json`: host name, time zone, locale, keyboard, system
packages and the unfree licenses accepted for them, Nyx apps, the services and groups
NyxOS manages, the system, boot and login themes) are read by the
NyxOS flake as its `nyxos-overrides` input. The manager builds a change before switching
to it, and puts the previous settings back if the build or the switch fails. It keeps
the settings each generation was built from, so a rollback brings them back too.

## The System Manager

Each method checks its caller with polkit, one action per kind of change
(`io.github.nyxos_project.system.{settings,packages,services,groups,firewall,clean,accounts,password,defaults,update,rollback}`),
and writes an audit line to the journal. Joining a group asks for an administrator every
time, since some (docker) are as good as root; only people's accounts are added. A change of
several kinds asks once: each kind's action is checked without asking (what polkit denies
stays denied), and what still needs a password is asked for together as
`io.github.nyxos_project.system.apply`. Clients send typed changes
(`nyx_system_proto::Change`), never commands. One change runs at a time.

```sh
nyx-system settings
nyx-system set time-zone America/Chicago
nyx-system install gimp
nyx-system install --accept-unfree vscode
nyx-system service printing on
nyx-system app mail on                         # a Nyx app
nyx-system group add docker ken
nyx-system update
nyx-system generations
nyx-system rollback
nyx-system default set appearance theme nord   # every new user's theme
nyx-system config show appearance              # merged, with each value's layer
```

## Software catalog

`crates/nyx-catalog/catalog.toml` is what Nyx Welcome, Nyx Store and `nyx-system catalog` offer, in
sections (Welcome's steps) and categories, with four presets (Everyday, Developer, AI
Developer, Homelab) that are starting selections. Items are in three tiers: base
(preinstalled), welcome and store. Each says what it installs (nixpkgs packages for
everyone, Nyx apps, a Flathub app for the user, services) and what it needs (groups, an
unfree license). A selection becomes one rebuild through the System Manager plus the
Flathub installs, after the user agrees to what needs agreeing to:

```sh
nyx-system catalog                             # * marks what's installed
nyx-system catalog install --preset developer docker
nyx-system catalog remove docker
```

NyxOS installs it at `/run/current-system/sw/share/nyx/catalog.toml`, and its flake
checks it against nixpkgs and its own configuration.

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
