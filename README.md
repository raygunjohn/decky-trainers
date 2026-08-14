# DeckyTrainers

A [Decky Loader](https://github.com/SteamDeckHomebrew/decky-loader) plugin that
**builds a game trainer on the Steam Deck from any Cheat Engine table**, then
drives its toggles and value editors from the Quick Access Menu.

The point: **a player never leaves Gamescope mode.** Pick a table, press a
button, flip cheats with the gamepad. No PC, no browser, no keyboard.

## Demo video

[![DeckyTrainers Demo Video](https://img.youtube.com/vi/SoVDkeHuqhU/maxresdefault.jpg)](https://youtu.be/SoVDkeHuqhU)

## Install

Requires [Decky Loader](https://github.com/SteamDeckHomebrew/decky-loader).

In the Decky menu (**…** → plug icon) → **⚙ Settings**, turn on **Developer
mode**, open the **Developer** tab, and paste this into **Install Plugin from
URL**:

```
https://github.com/raygunjohn/decky-trainers/releases/download/v0.1.1/DeckyTrainers.zip
```

Or download `DeckyTrainers.zip` from the
[releases page](https://github.com/raygunjohn/decky-trainers/releases), unzip into
`~/homebrew/plugins/`, and restart Decky.

## Using it

1. Launch the game from Gaming Mode, then open **…** → Decky →
   **DeckyTrainers**. The panel shows the running game and three tabs:
   **Local**, **Repo**, **Installed**.
2. **Local** — pick a downloaded `.ct` with the folder button. Trainer name and
   **Game process** are pre-filled from the running game. **Build trainer**
   lands it in `~/trainers`.
   **Repo** — paste `github.com/owner/repo` and tap **Load** to list and build
   from every `.ct` in a repository. (A **GitHub token** in the plugin settings
   is optional; it enables private repos and a higher rate limit.)
3. The panel switches to **Installed**. Tap ▶ to start the trainer inside the
   game's Proton prefix.
4. When the REST API answers, the feature list appears. **Attach** is pre-filled
   with the process name recorded at build time — press it.
5. Toggle features, close the QAM, keep playing. **KILL TRAINER** stops it at
   any time; it also stops automatically when the game exits.

## Build

For developers. Needs **Node.js v16.14+**, **pnpm v9**, and **Docker** — the
Decky CLI uses containers to assemble `py_modules` _and_ to compile the Rust
generator.

```bash
git clone https://github.com/raygunjohn/decky-trainers.git
cd decky-trainers
pnpm i

./scripts/build-deck.sh   # installs the Decky CLI on demand → out/DeckyTrainers.zip
pnpm build                # rollup only — fast frontend iteration, writes dist/
pnpm typecheck            # tsc --noEmit
```

`build-deck.sh` builds `backend/Dockerfile`, which produces **two binaries for
two different operating systems** and copies them into the zip's `bin/`:

| Artifact                   | Format            | Objective                               |
| -------------------------- | ----------------- | --------------------------------------- |
| `ct2trainer`               | x86-64 Linux ELF  | turning a `.CT` into a trainer          |
| `deckytrainers-x86_64.exe` | x86-64 Windows PE | trainer executable to run inside Proton |

## Deploy

Ships a built zip to a Deck over SSH and restarts Decky. Configure the
connection by copying the template — `.vscode/settings.json` is **gitignored**,
so it is the file allowed to hold a password:

```bash
cp .vscode/defsettings.json .vscode/settings.json
```

These environment variables override it: `DECK_IP` `DECK_PORT` `DECK_USER`
`DECK_PASS` `DECK_KEY` `DECK_DIR` `PLUGIN_NAME`.

```bash
./scripts/build-deploy-deck.sh   # build + deploy (or build-deck.sh / deploy-deck.sh alone)
./scripts/deploy-deck.sh --status        # what's deployed + loader state
./scripts/deploy-deck.sh --restart-only
```

## How it works

The trainer is a generated **Windows `.exe` that runs inside the game's own
Proton prefix**, launched through the same `wine` binary the game is using.

```
Deck (Linux)                          Game's Proton prefix (Windows)
┌───────────────────────┐             ┌──────────────────────────────┐
│ Decky QAM panel       │             │  trainer.exe                 │
│   ↕ callable          │             │    ├── .CT                   │
│ main.py (Plugin)      │──launches──▶│    ├── AA engine + Lua       │
│   └ bin/ct2trainer    │             │    └── REST :42069           │
│     (.CT → .exe)      │◀──HTTP──────│              ↕ injection     │
└───────────────────────┘             │           the game           │
                                      └──────────────────────────────┘
```

1. `bin/ct2trainer` (a **Linux** binary) takes a `.CT` and emits a **Windows
   PE** — the trainer runtime with the table appended as an overlay.
2. `main.py` launches that `.exe` through the game's own `wine` and prefix.
3. The trainer parses the table, runs its Lua, injects, and serves a REST API on
   `127.0.0.1:42069`.
4. The QAM panel renders that API's feature tree and posts back toggles.

## Limitations

- **Single-player games only**
- **One trainer at a time**
- **x86-64 only**
- **Global hotkeys from a table do not fire.** (todo)

## License

BSD-3-Clause — see [LICENSE](LICENSE). Scaffolded from
[decky-plugin-template](https://github.com/SteamDeckHomebrew/decky-plugin-template).
