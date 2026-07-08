# TUI Smoke Test

Use this checklist when changing terminal startup, input handling, mouse support,
or responsive layout behavior.

## Manual Checks

1. Run `cargo run -p bitbygit` in a normal terminal.
2. Confirm the app enters the alternate screen and shows repos, status, details,
   queue, and prompt panels.
3. Press `Tab` and `Shift+Tab`; focus should stay visible on every step.
4. Resize the terminal to a narrow or short size; focus should remain visible and
   the queue panel may disappear in compact mode.
5. Click each visible panel; focus should move to the clicked panel.
6. Click repo rows; the selected repo marker should move only when clicking
   inside the repo list content, not on borders.
7. Focus the prompt and type text; ordinary text should appear, modifier chords
   should not insert prompt text, and `q` should be inserted rather than exit.
8. With staged changes present, focus the prompt, enter `commit -m "smoke test"`,
   and confirm the visible commit plan can be cancelled with `n`.
9. Focus the prompt and enter `fetch` in a repository with a remote; confirm it
   runs and refreshes status.
10. Focus the prompt and enter `push`, `pull`, and `pull --rebase`; confirm
    visible risky plans can be cancelled with `n`.
11. Press `Esc`, `q` outside the prompt, and `Ctrl+C` in separate runs; each should
    exit cleanly.
12. After exit, confirm the shell prompt, cursor, mouse behavior, and terminal echo
    are restored.

## Automated Coverage

Unit tests cover focus cycling, compact layout behavior, prompt input guards,
mouse focus/selection, guarded commit and sync prompt parsing, branch summaries,
and render smoke tests with `ratatui`'s test backend.
