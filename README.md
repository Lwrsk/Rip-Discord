# RIP Discord

**English** · [Français](README.fr.md)

A lightweight, private Discord/TeamSpeak-style chat written in Rust. One
program: any instance can create a server, join one, or host it in place of
its creator. No central server, no account, your data stays on your PCs.

The app itself is in French for now.

## Install (Windows)

Download `RIP-Discord-Setup-x.y.z.exe` from the
[Releases](https://github.com/Lwrsk/Rip-Discord/releases) and run it. The
installer creates shortcuts (Start menu and desktop), allows the app through
the Windows firewall and adds an uninstall entry.

The installer is not code-signed yet, so Windows SmartScreen warns on first
launch.

## Features

- **Servers without a server**: whoever creates a server hosts it from their
  own PC, and any member can host it later with the same channels, history and
  invite code. If the host leaves or crashes, the next member in a known
  succession order takes over within seconds; nobody is disconnected.
- **Invite codes**, hidden by default so they don't leak on a screen share or
  stream. Paste one in « Rejoindre » (Join) and that's it.
- **Text channels**: replies, reactions, edits, deletions, `@mentions` (with
  Windows notifications), images and files up to 25 MB (10 per message, saved to
  Downloads on click, never opened automatically), anti-spam.
- **Voice channels**: Opus audio sent directly from PC to PC, noise
  suppression (RNNoise), echo cancellation and automatic gain (WebRTC ports),
  per-person volume, push-to-talk, global hotkeys that work in games, and
  **whisper groups** (hold a key to talk only to a group).
- **Screen sharing**: a whole screen or a single window
  (Windows.Graphics.Capture), with or without the cursor, with the app's sound
  (per-process WASAPI on Windows), H.264 at 10, 20 or 30 fps. Viewers opt in;
  switch source without cutting anyone off.
- **Webcam**: H.264, 640 px at 20 fps, thumbnails over the chat, full screen
  view.
- **Roles**: the creator names admins; the creator and admins edit channels
  and history settings, kick and ban.

### The server does not depend on its creator

Each member keeps a copy of the server (name, channels, admins, and history if
the server keeps it). In « Mes serveurs » (My servers), **any member can click
« Ouvrir » (Open)**: if a friend is already hosting, the app connects to them;
otherwise their own PC hosts, with the same channels, history and invite code.

Re-hosting does not give control of the server:

- the server id is bound to the creator's key;
- the admin list is signed by the creator;
- settings are signed by the creator or an admin;
- every message is signed by its author (a host can neither alter nor invent
  one);
- the ban list is signed by the creator or an admin, and a banned user cannot
  re-host;
- joining requires proving knowledge of the invite code, and the host must
  prove it too.

Copies fill each other in: on each connection, a member sends the host the
messages it has and the host lacks, and the host forwards them to everyone.

### Your data

Everything lives in one folder (`%APPDATA%\RIP Discord` on Windows,
`~/.local/share/RIP Discord` on Linux, or `RIP_DISCORD_DATA`):

| File | Content |
|---|---|
| `identity.key` | your private key (your identity): never share it |
| `servers/<id>/card.bin` | your signed copy of the server |
| `servers/<id>/history.bin` | kept messages, if the server keeps history |
| `servers/<id>/files/` | images and files sent or seen in this server |

« Oublier » (Forget) deletes a server's copy from your machine. If an admin
turns history off, nothing more is written to disk and existing copies are
erased on every member's PC.

## Do I need to open ports?

**No, and there is nothing else to install**: no server, no Raspberry Pi, your
PCs do the hosting.

1. **Decentralized directory**: to find out which member is hosting, the app
   uses the BitTorrent Mainline DHT. The host writes its address there,
   **encrypted** with a key derived from the invite code. Without the code you
   can neither find nor read it. No messages or voice go through it.
2. **UDP hole punching**: the joining friend writes their (encrypted) address
   to the directory too; the host sees it and sends a packet while the friend
   connects, so both routers let the traffic through. Public addresses are
   learnt from public STUN servers (Google, Cloudflare).
3. **UPnP**, as a bonus: if your router allows it, the app also opens the port.

Not supported yet: symmetric NAT (common on 4G/5G and some corporate networks;
the app tells you when it detects one) and networks that block all UDP. In
those cases, forward **UDP port 7878** on the host's router to the host's PC
and share the invite code as usual: a bare `IP:7878` is not enough, the host
requires the code (step-by-step guide in the
[French README](README.fr.md#tuto--ouvrir-le-port-à-la-main)).

## Security

What is protected:

- all traffic is QUIC (TLS 1.3), encrypted between each pair of PCs;
- **the invite code is a key**: to join, you prove that you know it without
  sending it. Someone who finds a host's address (by scanning the Internet,
  for example) cannot join without the code. The host must prove it too, so a
  fake address in the directory is exposed;
- messages, settings, admins, bans and the code are signed (Ed25519): neither
  a host nor a member can forge them;
- the DHT directory is encrypted with a key derived from the code;
- attachments are checked against their hash, never opened automatically, and
  marked as downloaded from the Internet on Windows (Windows warns before
  running a received program).

Known limits:

- **Whoever has the code is a member.** A former member who kept the code can
  come back, even after a ban, with a new identity. If the code leaked, or
  after a ban, **change it**: Server settings > Invitation > « Nouveau code ».
  Connected members get the new one automatically; the others have to ask for
  it.
- **The current host reads the messages** it relays (like any member): it can
  neither alter nor invent them, but this is not end-to-end encryption between
  author and readers.
- **Your IP address** is visible to other members (direct connections), as in
  any peer-to-peer software.
- The app has not had an external security audit.

Found a vulnerability? Please report it privately, see [SECURITY.md](SECURITY.md).

## Build and run

Requires stable Rust. On Linux, the CI installs
`libasound2-dev libpipewire-0.3-dev libclang-dev libxcb1-dev libxcb-randr0-dev
libxcb-shm0-dev libdbus-1-dev libwayland-dev libegl-dev libgbm-dev libx11-dev`.

```sh
cargo run                                       # graphical interface
cargo run -- host --name Alice                  # create a server, host it on 0.0.0.0:7878 and join it
cargo run -- servers                            # servers known on this machine
cargo run -- host --name Bob --server 9700      # re-host a known server (name or id prefix)
cargo run -- host --server 9700 --headless      # relay only
cargo run -- join G00E-0R9Q-3VZG-… --name Bob   # invite code or IP:port
cargo test
```

In the terminal UI, every line goes to the current text channel; commands are
`/salons`, `/salon <name>`, `/vocal <name>`, `/vocal` (leave), `/ping`,
`/peers`, `/quit`. Logs: `RUST_LOG=rip_discord=debug`.

To test offline, `cargo run -- dev-network` starts a local DHT and STUN server
and prints the `RIP_DISCORD_DHT_BOOTSTRAP` and `RIP_DISCORD_STUN` variables to
give the other instances. Without a webcam, `RIP_DISCORD_FAKE_CAMERA=1` shows
an animated test pattern.

The `Installeur Windows` workflow builds the NSIS installer
(`installer/rip-discord.nsi`) on every PR, and publishes a Release on `v*`
tags.

## Layout

```
src/
├── audio/      voice: mic, output (cpal), Opus, mixing, echo/noise removal, UI sounds
├── capture/    screen sharing (xcap, Windows.Graphics.Capture), H.264, app sound, webcam
├── core/       shared network core: protocol, signed server card, identity, storage,
│               files, invite codes, encrypted DHT directory, STUN, TLS
├── server/     host mode: relays text, voice and settings, succession, UPnP
├── client/     client, joining, direct voice mesh, host failover
└── ui/         egui interface (gui/) and terminal interface
tests/          end-to-end tests: sessions, messages, failover, direct voice, directory
installer/      NSIS script of the Windows installer
```

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at
your option.
