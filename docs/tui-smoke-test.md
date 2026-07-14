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
11. Focus the prompt and enter `branches`, `checkout <branch>`,
    `branch <name>`, `merge <branch>`, and `rebase <base>` in a clean test repo;
    confirm branch-changing plans are visible and cancellable with `n`.
12. On a GitHub-backed feature branch, enter `push and open pr`; confirm one plan
    previews the push followed by deferred PR planning, then confirm it pushes and
    creates or surfaces a PR URL.
13. With staged changes on a GitHub-backed feature branch, enter
    `commit -m "smoke test" and push and open pr`; confirm the preview shows all
    three steps, then confirm it commits, pushes, and creates or surfaces a PR URL.
14. Repeat the full prompt with a failing commit or push; confirm later steps stop
    and `gh` is not invoked. Repeat with `gh` missing or logged out; confirm the
    push completes, PR creation stops, and install or login guidance is shown.
15. Press `Esc`, `q` outside the prompt, and `Ctrl+C` in separate runs; each should
    exit cleanly.
16. After exit, confirm the shell prompt, cursor, mouse behavior, and terminal echo
    are restored.

## Automated Coverage

Unit tests cover focus cycling, compact layout behavior, prompt input guards,
mouse focus/selection, guarded commit, sync and branch prompt parsing, branch
summaries, typed branch workflow temp-repo behavior, sequenced commit/push/PR
short-circuiting with a fake `gh`, and render smoke tests with `ratatui`'s test
backend.
