# Updates on NyxOS

How NyxOS installs and updates software, what owns each piece, and how `nyx update` puts it
together. The audit below is how things stood before the unified command (November 2026);
the sections after it describe what replaced it.

## Before: what existed

- `nyx update` (the bash `nyx` in nyxos `pkgs/nyx-cli/nyx.sh`) ran `nix profile upgrade`
  on the user's own packages, nothing else.
- `nyx-system update` asked the System Manager's D-Bus `Update` method to run
  `nix flake update` on `/etc/nyxos/flake`, then `nixos-rebuild build` and `switch`, with
  snapper snapshots around it and the old `flake.lock` and generation put back on failure.
- Settings ran `nyx-system update`; the Store ran it and then `flatpak update --user`;
  Welcome called `Update` over D-Bus. Each had its own idea of "update everything".
- The host flake has one input, `nyxos/<channel>`, so nixpkgs, nyx-packages (every Nyx
  app) and the NyxOS modules arrive together, as one tested lock.
- The coding agents came from nixpkgs (Claude Code, Codex, Cursor Agent, Crush,
  OpenCode), so a newer one meant a newer nixpkgs: the whole system. `grok-build` wasn't
  in nixpkgs at all. Antigravity, ChatGPT and Herdr were packaged in nyxos with their
  versions written inline and bumped by hand.
- No self-updater was turned off anywhere; `services.fwupd` was on for physical machines
  but nothing called it; there was no update history besides the per-generation
  settings in `/var/lib/nyx-system/generations`.
- CI checked formatting and `nix flake check` on each push; nothing watched upstream
  releases.

## Who it's installed for

Apps and tools are each person's own. A catalog item's `scope` says who gets it:

- **For you** (the default): its packages, and its Nyx apps as `nyx-<name>`, go into the
  person's Nix profile from the system's channel, `github:nyxos-project/nyxos/<branch>#NAME`
  (the nyxos flake's `legacyPackages`: the channel's nixpkgs, its tool pins and its
  standalone Nyx apps; unfree only for what the catalog marks unfree, after the license is
  accepted). No password, no rebuild, other accounts unaffected; `nix profile upgrade`
  moves it to the channel's head, which CI built and cached.
- **For everyone**: base items, anything that turns on a service or adds a group, and the
  Nyx apps built into the desktop (Settings, Store, Welcome, Files, Capture, Backup, Share,
  Clipboard, Glimpse, Assistant, Activity, the shell). Through the System Manager, in one
  rebuild.

Names resolve one way everywhere (`Catalog::resolve`: id, name, aliases, command, packages,
Nyx apps), so `nyx install claude`, `nyx update claude`, `nyx remove claude` and
`nyx search claude` mean Claude Code; a name the catalog doesn't have is a package from the
channel, for the person.

Machines installed before this keep what NyxOS installed for everyone:
`nyxos.standaloneApps = "system"` (the default; the installer writes `"per-user"`) keeps
Notepad, Calculator, Archive and Photos enabled for everyone, and nothing moves by itself.
`nyx-system catalog move NAME` (Settings > Software, "Move to My Account") adds it to the
person's profile first, then removes it for everyone in one rebuild.

A Nyx app in someone's profile is trusted like the system's: the System Manager's
`TrustApps` adds the `bin` folders of the Nyx apps in the caller's profile that NyxOS's
cache signed to `/etc/nyx/trusted-bin` (Accounts, Clipboard, app actions and the shell
know programs by it; a person can't add a program of their own by building something
called nyx-mail). The `nyx-profile-units` user generator links their user units and the
ones they start with the session (`share/nyx/autostart`), with the PATH the system's get.
Each profile change, and each sign-in, runs both.

## Who owns what

Every piece of software has one way it's updated. Nothing else may change it.

| Software | Comes from | Updated by | Self-updater |
| --- | --- | --- | --- |
| NixOS, the kernel, NyxOS modules | nixpkgs and nyxos, through the channel | `nyx update system` | none |
| Nyx apps built into the desktop | nyx-packages, through the channel | `nyx update system` | none |
| Standalone Nyx apps, catalog apps, packages from `nyx install` | the channel, in the person's profile | `nyx update apps` / `nyx update NAME` (`nix profile upgrade`) | none |
| Claude Code (`claude`) | nyxos tool pin, Anthropic's release bucket | `nyx update claude` | `DISABLE_AUTOUPDATER=1` |
| Codex CLI (`codex`) | nyxos tool pin, GitHub release (OpenAI's package of static binaries) | `nyx update codex` | startup check off (`-c check_for_update_on_startup=false`) |
| Cursor Agent (`cursor-agent`) | nyxos tool pin, Cursor's download server | `nyx update cursor-agent` | `--disable-auto-update` |
| Grok Build (`grok`) | nyxos tool pin, xAI's release bucket | `nyx update grok` | `GROK_DISABLE_AUTOUPDATER=1` |
| Crush (`crush`) | nyxos tool pin, GitHub release | `nyx update crush` | none (only its provider list refreshes) |
| OpenCode (`opencode`) | nyxos tool pin, GitHub release | `nyx update opencode` | `OPENCODE_DISABLE_AUTOUPDATE=1` |
| Antigravity CLI (`agy`) | nyxos tool pin, GitHub release | `nyx update agy` | `AGY_CLI_DISABLE_AUTO_UPDATE=1` |
| Antigravity IDE | nyxos tool pin, Google's CDN | `nyx update antigravity-ide` | none on Linux tarballs |
| Google Antigravity (app) | nyxos tool pin, Google's AppImage | `nyx update antigravity-app` | none once extracted to the store |
| ChatGPT desktop | nyxos tool pin, OpenAI's Debian repository | `nyx update chatgpt-desktop` | none (the .deb relies on apt) |
| Herdr desktop | nyxos tool pin, releases.herdr.dev | `nyx update herdr-app` | off: its icon isn't beside the binary |
| oh-my-pi and other flakes in a user's profile | the user's Nix profile | `nyx update omp` (`nix profile upgrade`) | none |
| Flatpaks | Flathub, per user | `nyx update apps` / `nyx update NAME` | none |
| Languages and tools from mise (Node, Go, Zig, project CLIs) | the user's mise config, `~/.config/mise/config.toml` | `nyx update tools` / `nyx update NAME` (`mise upgrade`) | none (Nix's mise has none) |
| Python tools from `uv tool install` | the user's uv tools, `~/.local/bin` | `nyx update tools` / `nyx update NAME` (`uv tool upgrade`) | none |
| Firmware | fwupd and LVFS | `nyx update firmware`, always asked | n/a |

The tools are pinned in nyxos, and installed per person from the channel (so their
update is that person's `nix profile upgrade` to the channel's newer pin). A tool someone
installed for everyone (`nyx-system install`, or a machine from before) is updated by the
System Manager, as below.

Gemini CLI and Pi Forge aren't packaged: the catalog offers Antigravity CLI as Gemini
CLI's successor. A tool joins this table by getting a pin.

### mise and uv

System tools come from NyxOS; project toolchains come from mise and uv. mise gives a
project the Node, Python or Go version its `mise.toml` asks for, and new language
releases the day they're out; uv installs Python tools and manages projects' Python.
What they install is the user's: `nyx update tools` runs `mise upgrade` (within what the
user's global mise config allows: `node = "22"` stays on 22) and `uv tool upgrade --all`
as the user, in the home folder, so a project's own `mise.toml` is left to the project.
Neither is in a NixOS generation, so a system rollback doesn't undo them.

Their downloads are ordinary Linux programs, so NyxOS turns on `nix-ld`
(`modules/nixos/toolchains.nix`) with the libraries Node's and Python's native modules
use. zsh runs `mise activate` once mise is installed, and `~/.local/bin` is on the PATH.

Don't install the coding agents NyxOS packages with mise as well: two copies update
separately and whichever is first on the PATH runs. `nyx update status` says when it
finds one, and `nyx update NAME` lists both rather than guess.

### Exceptions

- Antigravity IDE and the Antigravity app have no machine-readable release feed. CI
  can't find their releases, so their pins are bumped by hand (`scripts/update-tool`
  says so instead of guessing).
- Cursor publishes no checksums; its pin's hash is what CI downloaded over HTTPS from
  `downloads.cursor.com`. Grok's is checked against the bucket's MD5. Every other tool's
  hash is the one its vendor published (GitHub's asset digest, Anthropic's manifest,
  OpenAI's apt index, Herdr's `latest.json`).

## Tool pins

The tools NyxOS packages itself have their versions in nyxos `pkgs/tools/sources.json`:
version, a `recipe` number, the download URL and hash, where releases are found and the
names the tool answers to. `recipe` goes up whenever the Nix code that builds a tool
changes, so a pin is only ever built by the recipe it was validated with.

A system builds its tools from the channel's pins, except where
`/etc/nyxos/overrides/tools.json` has a newer one with the same recipe. That file is how
one tool installed for everyone is updated on its own: `nyx update claude` copies Claude's
pin from the head of the channel into it and rebuilds, so only Claude changes. A system update makes the
channel's pins current again and empties the file. The effective pins, and which tools
are installed, are in `/etc/nyxos/tools.json`.

## Package CI

nyxos `.github/workflows/tool-updates.yml` runs every six hours (and by hand, for one tool
or all):

1. `propose` (read-only, no secrets), once per tool: `scripts/update-tool NAME` asks the
   upstream's release feed for its newest stable release, verifies the hash against what
   the vendor published, rewrites that tool's pin, builds it and runs its smoke check
   (`--version`, `--help`, nothing written to `HOME` that could update it).
2. `publish` (may write; builds nothing): a passing update becomes a pull request on
   `tool-update/NAME`, force-pushed so there's only ever one per tool; a failing one opens
   or updates the issue "Tool update failed: NAME", and the last validated pin stays.

A pin's `update` object sets how CI treats it: `"enabled": false` keeps CI off it (and
`upstream.kind = "manual"` tools have no feed to check), and `"autoMerge": true` lets a
release that isn't a new major version merge itself once the required checks pass
(`gh pr merge --auto`; branch protection decides which checks). Every pin starts with
auto-merge off: a person reviews each PR until a tool has a record of clean updates, and
breaking packaging changes (a new `recipe`) are never a pin-only PR. A pull request merged
by the workflow's token starts no CI on `main`, so `publish` starts it when `main`'s head has
none: that run is what fills the cache.

A merged PR lands on `main` (testing); `stable` moves when the known-good process promotes
it, and that's when stable machines see it.

Every action in the workflows is pinned to a commit (Dependabot proposes new ones), and a
`tool-update/*` build gets no Cachix token at all: it runs a new upstream binary.

Generic updaters (`nix-update`) don't fit these pins: they're JSON with vendor manifests,
and `update-tool` checks each hash against what the vendor published. x86_64-linux is the
only platform today; `srcs` is keyed by system for when aarch64 has runners and releases.

### Adding a tool

1. A recipe in nyxos `pkgs/NAME/default.nix` reading `pin.version` and
   `pin.srcs.${system}`, with its self-updater turned off, and a line in
   `pkgs/tools/overlay.nix`.
2. Its pin in `pkgs/tools/sources.json` (`name`, `version`, `recipe: 1`, `aliases`, `srcs`,
   `upstream`), and an `upstream.kind` `scripts/update-tool` knows (or `manual`).
3. Its smoke test in `checks/tools.nix` (`smoke.NAME`: its command, the arguments for its
   version and help, the environment it needs offline), or none for a desktop app.
4. A catalog item in nyx-system's `catalog.toml` (`packages = ["NAME"]`, `unfree` as
   nixpkgs would say, `bin`, `aliases`), in `coding-agents` or `agent-apps` to count as a
   tool.

`publish` uses the run's own token, so there's no secret to keep: the repository allows
Actions to create pull requests, and since a pull request opened that way starts no CI
by itself, `publish` starts CI on the branch (workflow_dispatch) and its result shows on
the pull request. Cachix doesn't push from `tool-update/*` branches, so upstream
binaries reach the cache only after review.

## `nyx update`

```text
nyx update                 everything (firmware is checked and asked about)
nyx update system          the channel: NixOS, NyxOS, the Nyx apps built into the desktop
nyx update apps            your own apps (your Nix profile) and Flatpaks
nyx update tools           coding agents (yours; pinned ones for everyone in one rebuild), your mise and uv tools
nyx update firmware        fwupd devices, always asked
nyx update NAME…           what NAME is, wherever it comes from
nyx update … --dry-run     the plan only
nyx update … --yes         don't ask
nyx update … --json        the plan or result as JSON
nyx update check           what has updates (offline: the last check's, and when)
nyx update status          everything installed: version, what's newer, how, its update setting
nyx update auto [--now]    the daily run (see Update settings)
nyx update history         what was updated, when, how long it took and how it went
nyx update report          failures, rollbacks, restarts and typical times
```

Profile packages from the channel have real versions: what's installed comes from the
store path, what's new from the channel's head (a tool's pin, or one evaluation of the
other packages' versions at the head's revision), and only what has news is upgraded.
Afterwards the programs still running from what was replaced are named ("reopen X");
nothing is stopped, and Nix never replaces a running binary in place.

`system`, `apps`, `tools`, `firmware`, `check`, `status`, `history`, `report`, `auto` and
`help` are never package names. A name that fits several things is asked about on a
terminal (a numbered list) and otherwise fails with the list.

Exit codes, for scripts: 0 done or nothing to do, 1 failed, 2 usage, 3 partly updated
(what failed was left as it was), 4 a name that isn't installed or fits several things, 5
no network, 6 another update or system change running, 7 declined. With `--json` a failed
step is in the outcome and the code stays 0 (Settings and the Store read the outcome).

`nyx-system update` is the same command. Names resolve across everything: tool ids,
the names their commands have, catalog ids, Nyx apps, profile packages and Flatpaks.

The engine is the `nyx-update` crate, shared by the command, Settings and the Store.
System-level steps (the channel and tool pins) go to the System Manager's `Upgrade`
method as one request, so one rebuild covers them; the manager reads the pins from the
channel itself and never builds a URL a client sent. User steps (`nix profile`,
`flatpak --user`, `mise upgrade`, `uv tool upgrade`) run as the user. Firmware goes through `fwupdmgr`, which asks polkit
itself.

## Safety

- A build that fails never switches; a switch that fails goes back a generation;
  `flake.lock` and `overrides/tools.json` are put back either way.
- If the manager is stopped mid-update (power cut), it puts them back when it next
  starts, from `/var/lib/nyx-system/update-in-progress`.
- Under 3 GB free in `/nix`, an update doesn't start.
- One change at a time: the manager's lock for system steps, a per-user lock for the
  rest (and Nix's own lock on a profile).
- A profile upgrade keeps the previous profile generation until garbage collection:
  `nyx rollback` (Settings > Updates, "Your apps and tools") puts it back. That's separate
  from the system's generations, and neither covers firmware. There's no rollback across
  the system, apps, tools and firmware together.
- The kernel track and the boot loader don't change with an update; a kernel, initrd or
  systemd change is reported as needing a restart, and apps still running from replaced
  files are listed.
- Old generations stay until garbage collection removes them by its own policy.
- Firmware is never part of an unattended run and is never called reversible.

## History

System steps: `/var/lib/nyx-system/history.jsonl` (the manager's; last 200). User steps:
`~/.local/state/nyx/update-history.jsonl` (last 200). `nyx update history` shows both.
Each record has the time, the request, the targets, old and new versions, the
generation before and after, the result, whether a restart is needed, the error, and how
long each stage took: `check` (finding what's new), `pins`, `download` (the channel),
`build` (which also fetches what the new system needs), `switch` and `total`. Going back
a generation is recorded too (`rolled-back`).

`nyx update report [N]` reads the last N (200) records: system updates that worked,
failed (and the failure rate) or found nothing new, rollbacks and whether they worked,
the user's own updates, restarts needed, the typical (median) time of each stage, and the
longest update. `--json` gives the same.

## Update settings

Each person's, in the `updates` domain (`nyx-system config set updates KEY VALUE`, or
Settings > Updates):

| Key | Values | Default |
| --- | --- | --- |
| `system` | `off`, `check`, `notify`, `auto` | `notify` |
| `apps` | the same | `auto` |
| `tools` | the same | `auto` |
| `firmware-policy` | the same | `notify` |
| `install-at` | `"HH:MM"`, when automatic installs run | unset: when found |
| `check` | `false` turns every kind off | `true` |
| `notify` | notifications for news, automatic installs and failures | `true` |
| `check-on-metered` | check and install on metered connections | `false` |
| `firmware` | include firmware in checks | `true` |

`off` doesn't check; `check` shows it in Settings and `nyx update status` without a
notification; `notify` notifies and waits for the person; `auto` installs it. Only the
person's own software installs unattended (their profile, Flatpaks, mise and uv): for the
system and firmware, `auto` means `notify`, so an unattended run never rebuilds the
system, asks for a password or flashes firmware.

The `nyx-update-check` user timer runs `nyx-system update auto` daily: it installs what's
set to `auto` (with `install-at`, a one-time `nyx-update-install` user timer waits for
that time), notifies about the rest once per set of updates, notifies when an automatic
install fails (what failed is left as it was), and keeps the check in
`~/.cache/nyx/update-status.json` for Settings' "last checked" and for when there's no
network. It waits out metered connections unless allowed and does nothing when `check` is
off.
