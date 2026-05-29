# Launch Posts

## Where To Launch

Start here:

- GitHub release notes
- GitHub Discussions
- `r/ClaudeCode`
- DEV
- X

Then use these for a stronger feature drop:

- Show HN
- Lobsters

Reason:

- GitHub, Reddit, DEV, and X are the best fit for the current audience and project size
- Show HN and Lobsters work better when the release has a sharp technical hook and fresh demo

## Launch Order

### Day 1

- Publish the release
- Update the README and landing page
- Open a GitHub Discussion
- Post to `r/ClaudeCode`
- Post to X

### Day 2

- Publish the DEV post
- Reply to relevant Claude Code threads with the demo GIF

### Next feature release

- Post to Show HN
- Post to Lobsters

## GitHub Discussion

Title:

`claudectl`: orchestrate a swarm of Claude Code agents with a local-LLM brain that learns from you

Body:

I kept losing track of which Claude Code session was blocked, waiting for approval, or quietly burning budget, so I built `claudectl`.

It is a local dashboard for supervising multiple Claude Code sessions from one terminal.

Useful if you run several sessions at once and want to:

- see every session at once
- approve prompts without tab hunting
- enforce spend budgets
- jump to the right terminal quickly
- run dependency-ordered task graphs

Quick start:

```bash
brew install mercurialsolo/tap/claudectl
claudectl --demo
```

Repo:

- https://github.com/mercurialsolo/claudectl

## Reddit

Subreddit:

- `r/ClaudeCode`

Title:

I built a terminal dashboard for supervising multiple Claude Code sessions

Body:

Running multiple Claude Code tabs and losing track of which one is blocked, waiting for approval, or quietly burning money got old fast.

So I built `claudectl`, a local dashboard for supervising Claude Code from one terminal.

It is useful if you run several sessions at once and want to:

- see every session at once
- approve prompts without tab hunting
- set spend budgets and auto-kill over-budget runs
- jump to the right terminal quickly
- record demo GIFs from the dashboard

Quick start:

```bash
brew install mercurialsolo/tap/claudectl
claudectl --demo
```

Repo:

- https://github.com/mercurialsolo/claudectl

If this matches how you use Claude Code, I’d like to know what breaks first for you once you have 3 or more sessions open.

## DEV

Title:

Orchestrate a swarm of Claude Code agents with a local-LLM brain that learns from you

Body:

When I started running several Claude Code sessions in parallel, the operational failure mode was obvious:

- one tab was blocked on a permission prompt
- another was chewing through budget
- another looked alive but was actually stalled

Claude Code is strong at execution. It is not built to supervise five terminals at once.

So I built `claudectl`, a local operator layer for Claude Code. It gives me one dashboard to:

- see every session
- approve prompts without tab hunting
- control spend with budgets and auto-kill
- jump to the right terminal
- coordinate multi-session task graphs

Quick start:

```bash
brew install mercurialsolo/tap/claudectl
claudectl --demo
```

Repo:

- https://github.com/mercurialsolo/claudectl

## X

Post:

I got tired of tab-hunting 5 Claude Code sessions, so I built `claudectl`.

It shows which agent is blocked, waiting for approval, over budget, or stalled, and lets me intervene from one terminal dashboard.

```bash
brew install mercurialsolo/tap/claudectl
claudectl --demo
```

Repo:
https://github.com/mercurialsolo/claudectl

Attach:

- `docs/assets/github-social-preview.png`
- or a short GIF from `docs/assets/claudectl-demo-hero.gif`

## Show HN

Title:

Show HN: claudectl – orchestrate a swarm of Claude Code agents with a local-LLM brain that learns from you

Body:

If you run several Claude Code sessions at once, `claudectl` shows which one is blocked, waiting for approval, over budget, or stalled, and lets you intervene from one terminal dashboard.

It is local-only, zero-config, and currently supports macOS and Linux terminals including Ghostty, tmux, Kitty, Warp, iTerm2, and GNOME Terminal.

Quick start:

```bash
brew install mercurialsolo/tap/claudectl
claudectl --demo
```

Repo:

- https://github.com/mercurialsolo/claudectl

## Lobsters

Title:

claudectl: a terminal control plane for Claude Code sessions

Summary:

`claudectl` is a Rust CLI for supervising multiple Claude Code sessions from one terminal. It tracks session state, surfaces blocked prompts and spend, supports terminal switching and input for supported terminals, and can orchestrate dependency-ordered task graphs.

Best attached artifact:

- the demo GIF
- or a short architecture comment describing how local session discovery works
