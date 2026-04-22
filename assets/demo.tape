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

# Initial state: session 1 selected. Timeline shows the live WebSearch row
# animating — a tool that's currently running.
Sleep 3s

# Navigate down — session 2 has no tool calls, so the timeline slot is empty;
# session 3 repopulates it and also shows a live "Think" row (the model is
# generating its next reply, no tool_use yet).
Type "j"
Sleep 1.2s
Type "j"
Sleep 3.5s

# Press L to enter focus mode — detail area grows and the timeline takes the
# full width, so the Think row is easy to see growing.
Type "l"
Sleep 4s

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
