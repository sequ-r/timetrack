# The terminal client is one-shot CLI only; the TUI is removed

The terminal client is used between other work in a terminal — `timetrack quick -m 15` between two `git` commands on another project — not as a second full-screen home (REQUIREMENTS §18). So the ratatui TUI is removed: `timetrack` is one-shot subcommands that print one result and exit, bare `timetrack` prints `status` (week total first, the §11 hero rule in text), and `status`/`list` gain `--json` for scripts and status bars. The one-shot surface already covered every TUI action before this decision, so the removal costs the full-screen mode and ~1,950 lines plus two dependencies, and nothing else. `timetrack tui` is a breaking removal announced in release notes, not an alias.

## Considered Options

- **Keep the TUI beside the CLI.** Rejected: two terminal surfaces to keep at parity for one usage pattern, and the TUI's dialogs duplicate the GUI's reason to exist.
- **Bare `timetrack` prints help.** Rejected: the between-commands glance the author actually wants is the week total; help is one `--help` away.
