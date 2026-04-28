# hazmat: in-sandbox claude orphaned on host TTY disconnect

## Summary
When the host iTerm2 tab running `hazmat claude --docker=sandbox` is closed,
the in-sandbox `claude` process is not signalled. It survives indefinitely
on a dead pty (state `Ssl+`, no zombie), holding session state and
container resources until manually killed. Every closed tab leaves a live
orphan; over a few days a heavy user can accumulate 40+.

## Environment
- macOS 14.x (Darwin 25.2.0), iTerm2 3.6.6
- hazmat 0.6.0 (Homebrew: /opt/homebrew/Cellar/hazmat/0.6.0/bin/hazmat)
- sbx (Docker Sandboxes daemon) v0.26.1
- Sandbox image based on `docker/sandbox-templates:claude-code` (Linux 6.12.44 aarch64, tini PID 1)

## Reproduce
1. Open a fresh iTerm2 tab.
2. Run `hazmat claude --docker=sandbox` (any project).
3. Inside the sandbox shell, identify the in-VM claude PID (`ps -ax | grep claude`).
4. Close the iTerm2 tab via Cmd-W (do not `/exit` first).
5. From outside: `sbx exec linera-agent kill -0 <pid>; echo $?`
6. Expected: `1` (no such process — claude was signalled and exited).
   Actual: `0` (claude is still alive).

## Diagnostic data
The in-sandbox `claude` is left in `Ssl+` state. Per `/proc/<pid>/status`:
- `SigIgn` includes only SIGPIPE (bit 12); SIGHUP is not ignored.
- `SigBlk = 0`, `SigCgt = 0` for SIGHUP.
- Default disposition would terminate; therefore SIGHUP is never delivered.

That implies the kernel sees the pty master as still open. From inside
the container's PID namespace, no process is visible holding the ptmx fd
corresponding to the slave (`/dev/pts/N`) that claude is on. The master
is held by something outside the container's visible namespaces —
presumably the sbx daemon process that brokers the docker exec session
— and that process does not close its fd when the host client disconnects.

We also verified that arming `PR_SET_PDEATHSIG` (`setpriv --pdeathsig HUP -- "$@"`)
inside the sandbox-bootstrap chain does NOT cause claude to die on host
disconnect. PPID inside the container is `0` (parent in another namespace),
and that parent task does not die on client disconnect either — consistent
with the master-fd-still-held hypothesis.

## Expected behaviour
When the `hazmat`/`sbx` client disconnects (host tab close, hazmat SIGHUP, etc.),
the corresponding container-side exec target (or its descendants in the
foreground process group) should receive SIGHUP via the kernel pty hangup
mechanism. The sbx daemon should close its master end on client disconnect;
this would deliver SIGHUP to the foreground process group of the slave
naturally.

## Workaround
Periodic external reaper that diffs host `SANDBOX_HOST_TTY` env values
(open set) against in-container `*.terminal.json` sidecars (sandbox set)
and SIGHUPs alive members of the difference. Implementation: `src/reaper.rs`
in this repo, exposed as `claudectl --reap-orphans` and auto-runnable via
`claudectl --install-reaper` (macOS launchd / Linux systemd-user timer).

## Related
- moby/moby#9098 — `docker exec`: SIGHUP propagation on client disconnect.
