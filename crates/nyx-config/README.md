# nyx-config

NyxOS's configuration layers. A *domain* is one file name (`dock`, `shell`,
`appearance`…) in three folders, merged key by key, later ones winning:

| Layer | Folder | Written by |
| --- | --- | --- |
| Defaults | `/etc/nyxos/defaults` (`NYX_CONFIG_DEFAULTS`) | the NixOS configuration; read-only |
| System | `/etc/nyxos/overrides` (`NYX_CONFIG_SYSTEM`) | the System Manager (`SetDefault`, polkit) |
| User | `$XDG_CONFIG_HOME/nyx` | the user's programs |

Tables merge; any other value replaces the one below it.

## `Layers`

The files themselves:

- `read`, `merged`: one layer, or all of them merged.
- `get`: a value with the layer it came from.
- `set`, `reset`: one key in a writable layer; reset lets the layer below show through.

`nyx-system config get|set|reset|show` is the same from a terminal.

## `Store`

One component's domain, versioned. The component's built-in values are the lowest
layer.

- `load()`: every layer, with the user's file migrated to the current `schema` (copied
  to `DOMAIN.toml.vN.bak` first), or set aside as `DOMAIN.toml.broken-<time>` if it
  isn't TOML. What went wrong is in `Loaded::problems`.
- `save(&table)`: writes only what differs from the layers below, so values left alone
  follow NyxOS's defaults when they change. `drop_defaults` is the migration that turns
  a file that held everything into one like that.
- `set` / `reset` / `reset_all`: one key, or everything (kept as `.reset.bak`).
- `explain(key)`: the value in effect, its layer, what a reset leaves, and the user's
  own.
- `watch(f)` (feature `watch`): told when the domain changes in any layer, including a
  NixOS switch replacing `/etc/static`.
- `parse::<T>(&loaded)`: the merged table as the component's type; a key of the wrong
  type falls back to the built-in value on its own.

The component owns its domain's schema. Other programs (Nyx Settings, `nyx plugin`)
set and reset single keys through `Layers`, without stamping the file's schema.

How the desktop uses this is in nyx-settings' `docs/configuration.md`.
