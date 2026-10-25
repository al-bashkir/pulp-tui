# Demo GIF for the README (https://github.com/charmbracelet/vhs).
# Record again from a clean Pulp, in the repository root:
#   tests/env/pulp-env.sh down pulp1
#   vhs docs/demo/demo.tape

Output docs/demo/demo.gif

Set Shell "bash"
Set FontSize 16
Set Width 1600
Set Height 900
Set Padding 12
Set Framerate 25
Set TypingSpeed 50ms

# Test-only Pulp (admin/password) with two unsynced repositories, and a temporary data dir.
Hide
Type "cargo build --release -q && tests/fixtures/build.sh && tests/env/pulp-env.sh up pulp1 && tests/env/pulp-env.sh http-up && docs/demo/seed.sh" Enter
Wait@600s
Type 'export PATH="$PWD/target/release:$PATH" PULP_PASSWORD=password PULP_TUI_DATA_DIR="$(mktemp -d)"' Enter
Type "clear" Enter
Show

Type "pulp-tui --config examples/config.toml --profile lab"
Sleep 800ms
Enter
# A fresh Pulp answers the first Dashboard requests slowly.
Wait+Screen@60s /Statistics/
Sleep 3s

# Screen list: Dashboard -> Servers -> RPM Repositories.
Type "j"
Sleep 500ms
Type "j"
Sleep 500ms
Type "l"
Sleep 2.5s

# Sync from the default remote: action menu, f, the form is already filled.
Enter
Sleep 2s
Type "f"
Sleep 2.5s
Ctrl+S
Sleep 6s

# Content of the new version, then the versions of the repository.
Type "o"
Sleep 3.5s
Escape
Sleep 1s
Type "v"
Sleep 3s
Escape
Sleep 1s

# Command palette to the Task Center.
Type ":"
Sleep 500ms
Type "tasks"
Sleep 800ms
Enter
Sleep 3.5s

Type "?"
Sleep 2.5s
Escape
Sleep 800ms
Type "q"
Sleep 1s
