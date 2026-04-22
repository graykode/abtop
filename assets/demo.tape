Output demo.gif

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

# Initial state: session 1 selected, tool timeline visible on the right half
# of the detail pane (new default on wide terminals).
Sleep 2.5s

# Navigate down — session 2 has no tool calls, so the timeline slot is empty;
# session 3 repopulates it.
Type "j"
Sleep 1.2s
Type "j"
Sleep 2s

# Press L to enter focus mode — detail area grows and the timeline takes the
# full width.
Type "l"
Sleep 3s

# Press L again to return to the default side-by-side layout.
Type "l"
Sleep 1.5s

# Scroll back up.
Type "k"
Sleep 1s
Type "k"
Sleep 2s

Hide
Type "q"
