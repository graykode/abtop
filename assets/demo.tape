Output assets/demo.gif
Output assets/demo.mp4

Set Shell "bash"
Set FontSize 24
Set Width 2200
Set Height 1200
Set Padding 10
Set Theme "Catppuccin Mocha"
Set Framerate 30

Hide
Type "unset NO_COLOR; ./target/release/abtop --demo"
Enter
Sleep 1s
Show

# Initial state: session 1 selected with its chat and lifecycle evidence visible.
Sleep 3s

# Navigate through the demo sessions to show provider and status differences.
Type "j"
Sleep 1.2s
Type "j"
Sleep 3.5s

# Press L to replace chat with the selected session's tool timeline.
Type "l"
Sleep 4s

# Press L again to return to chat.
Type "l"
Sleep 1.5s

# Scroll back up.
Type "k"
Sleep 1s
Type "k"
Sleep 2s

Hide
Type "q"
