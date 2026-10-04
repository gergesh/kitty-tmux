# kitty-tmux

Runs a command on a pseudo-terminal and wraps the kitty graphics commands in its
output in tmux passthrough, so its images reach the terminal outside tmux.

It was written for one bug: **Claude Code shows no images inside tmux.**

## The bug

Claude Code draws images with the kitty graphics protocol's Unicode
placeholders. It sends the picture once as an APC string, `ESC _ G … ESC \`,
then prints placeholder characters where the picture should go. In kitty or
Ghostty that works.

Inside tmux it doesn't. tmux drops APC strings it does not understand, so the
outer terminal never receives the picture. The placeholder characters are plain
text, so they do arrive, and you get a blank box where the image should be.
`CLAUDE_CODE_FORCE_TERMINAL_IMAGES=1` doesn't help: it only overrides Claude
Code's check for image support and changes nothing about how the image is sent.

tmux has a way through. With `allow-passthrough on`, it forwards the payload of
`ESC P tmux; … ESC \` to the outer terminal, with each ESC inside doubled.
Claude Code already wraps its clipboard escapes this way but not its images.
kitty-tmux wraps the images too, on the way out.

## Install

```
cargo install --git https://github.com/gergesh/kitty-tmux
```

In `~/.tmux.conf`:

```
set -g allow-passthrough on
```

## Use

```
kitty-tmux claude
```

Or always:

```sh
alias claude='kitty-tmux claude'
```

Outside tmux, or when stdin or stdout is not a terminal (`claude -p … | jq`),
kitty-tmux execs the command directly and stays out of the way. Arguments go to
the command untouched; put `--` first if the command itself starts with `-`.

The child gets the outer terminal's size and termios settings, and follows
resizes. SIGINT, SIGTERM, SIGHUP and SIGQUIT are forwarded to it. kitty-tmux
exits with the child's status.

## Requirements

- tmux with `allow-passthrough on` (tmux 3.3 or later)
- An outer terminal that supports kitty graphics with Unicode placeholders, such
  as kitty or Ghostty
- macOS or Linux

## Related

[decset-rewrite](https://github.com/NightMachinery/decset-rewrite) uses the same
pty-proxy approach to rewrite mouse modes for Termux.
