---
name: Hive
description: Start, grow and check a claudectl hive across machines. Use when the user wants to start a hive, invite or add a machine or person, join someone's hive, approve a join request, see which machines are connected, share a skill with the hive, or asks why a peer shows no knowledge or will not connect. Covers `claudectl relay` (the connection between machines) and `claudectl hive` (the knowledge shared over it).
version: 0.92.1
---

# claudectl Hive

claudectl links machines in two layers. Keep them apart when you explain or act:

- **Relay** connects machines: encrypted TCP links made by pairing. It carries work and state: `relay fleet` (every session on every machine), `relay delegate`, `relay send-session`.
- **Hive** is the knowledge shared over those links: distilled approve/deny preferences, insights, and skills the user chose to share. Peers gossip it and weight it by trust; local knowledge wins a conflict.

The hive needs the relay. The relay does not need the hive. An **unnamed** hive is simply everyone this machine has paired with; naming it gives it membership rules (a join policy) on top of pairing.

## First, look

Run these before changing anything, and report what they say:

```bash
claudectl --version          # both machines should run the same version
claudectl hive identity      # "unnamed", or the hive's name and join policy
claudectl hive status        # membership, units, gossip sync per peer
claudectl relay peers        # paired machines, address, last heartbeat
```

Is a relay already listening? On macOS the background agent is `io.claudectl.relay`:

```bash
launchctl list | grep io.claudectl.relay
lsof -nP -iTCP -sTCP:LISTEN | grep claudectl
```

## Start a hive (the host)

1. Keep a relay running. Prefer the background agent over a foreground `relay serve`, because it survives logout:
   ```bash
   claudectl relay install-agent
   ```
2. Name the hive. `ask` makes every join wait for the owner's approval, which is the right default when anyone other than the owner will join:
   ```bash
   claudectl hive identity set --name <name> --description "<what it is for>" --join-policy ask
   ```
   Never pick `--join-policy open` on your own initiative. It lets any machine that sees the LAN broadcast join without approval. Only set it when the user asks for it by name.
3. Mint an invite and hand it to the user:
   ```bash
   claudectl hive invite --words      # link, relay code and word phrase
   claudectl hive invite --qr         # QR of the link
   ```
   The invite carries the pairing secret. Give it to the user to pass on privately. Do not post it to a channel, issue, PR or any shared document.

## Join a hive (the other machine)

```bash
claudectl hive join <link | relay code | word phrase>
claudectl relay install-agent                     # keep this side's relay up too
```

`hive join` dials the address inside the invite, pairs, records membership and exits. If it cannot reach that address it fails with `connection failed` and saves nothing, so the joiner must be able to reach the host's address as printed in the invite.

The joiner then has to dial the host for knowledge to move. `join` prints the line to do it:

```bash
claudectl relay connect <host-address>:<port>
```

`relay connect` stays in the foreground to hold the link open and only stops on ctrl-c. Ask the user to run it in its own terminal, or start it as a background command. Never run it inline before other steps: it will not return. `claudectl hive status` then reads `asked to join "<name>" ... awaiting owner approval` until the host approves.

To pair a machine without joining a named hive (only to delegate work or move sessions), use `claudectl relay invite` on one side and `claudectl relay join <code>` on the other.

## Approve who gets in (the host)

```bash
claudectl hive requests                     # who is waiting, and who is in
claudectl hive requests approve <peer-id>
claudectl hive requests deny <peer-id>
```

Approving and denying are the owner's decisions. Show the user the waiting list and let them choose. A denial cannot be undone by the peer asking again, so never deny a request just to clear it.

## Share a skill with the hive

```bash
claudectl hive share skill <path/to/SKILL.md>    # --scope universal | language:X | project:X
claudectl hive shared                             # what peers have shared
claudectl hive pending                            # received, not yet accepted
```

Installing something a peer shared runs that peer's instructions on this machine. List it with `hive pending` and let the user accept it.

## In the TUI

- `p` on the dashboard toggles the peers panel: state, trust, units sent and received, sessions per peer.
- `K`, then `Tab`, opens the Hive tab: `h` start listener, `i` invite, `J` join, `r` refresh.
- `S` on a session sends that conversation to a paired machine.

## When something looks wrong

- **`Address already in use` from `relay serve`.** A relay is already listening, almost always the launchd agent. Nothing to fix. Skip `relay serve` and go on to the invite.
- **The invite shows a LAN address and the other person is elsewhere.** A `192.168.x.x` address only works on the same network, and the invite cannot be pointed at another address. Joining with that code fails before anything is saved, and `relay connect` to a Tailscale address afterwards finds no pairing key. Tell the user the joiner has to be on the host's network (or reach that LAN address through a VPN route) when they redeem it.
- **A peer shows `0 units` and `last sync epoch 0`.** That means nothing has been sent yet, not that the peer is disconnected. Check `relay peers` for a recent heartbeat and `hive requests` on the host for membership. Units only exist once the brain has distilled some decisions.
- **Names like `peer-a`, `peer-b`, `host-1`, `ku_local`, `ku_remote` or `ku_1` in `hive status`, `trust.json` or `~/.claudectl/hive/conflicts.jsonl`.** These are test fixtures written by `cargo test` into the real `~/.claudectl` (#603), not peers and not real conflicts. Tell the user. Do not delete them without asking, and back up `~/.claudectl/hive` before anyone does.
- **A join between two of the user's own machines is silently closed while both relays run (#608).** Stop the joining machine's relay for the join, then start it again. On macOS the agent is `io.claudectl.relay`; ask the user before stopping it:
  ```bash
  launchctl bootout gui/$(id -u)/io.claudectl.relay                                # stop
  claudectl hive join <invite>
  launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/io.claudectl.relay.plist  # start again
  ```
- **A command from this skill is missing.** The machine runs an older claudectl. Compare `claudectl --version` on both sides and upgrade (`brew upgrade claudectl`).

Run `claudectl doctor` for a full install and runtime check.
