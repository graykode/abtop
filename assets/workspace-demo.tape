Output workspace-demo.gif

Set Shell "bash"
Set FontSize 24
Set Width 2200
Set Height 1200
Set Padding 10
Set Theme "Catppuccin Mocha"
Set Framerate 30

Hide
Type "../target/debug/abtop --demo"
Enter
Sleep 1s
Show

# Desktop default view.
Sleep 2s

# Open Agentic Workspace focus. This verifies the `a` shortcut in wide
# desktop layouts, not only in compact/narrow layouts.
Type "a"
Sleep 1s

# Move project selection inside the Workspace focus view.
Type "j"
Sleep 2s

# Open the selected project's first session in the regular sessions view.
Enter
Sleep 2s

Hide
Type "q"
