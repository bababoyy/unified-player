# README demo: Home → playlist → filter and play → queue → synced lyrics
# → YouTube Music → live theme preview. Synthetic showcase data in a
# throwaway config folder: nothing is signed in, streamed, or saved.
#
# Run from the repository root after `scripts/dev.sh build`:
#   vhs docs/assets/demo.tape
# It also refreshes the stills in docs/assets/screenshots/.

Output docs/assets/demo.gif
Require bash

Set Shell bash
# 18px on a 1300px canvas stays readable once GitHub scales it to its
# README column, and keeps the standard (not compact) workspace layout.
Set FontSize 18
Set Width 1300
Set Height 820
Set Padding 0
Set Margin 24
Set MarginFill "#101827"
Set WindowBar Colorful
Set BorderRadius 10
Set Framerate 15
Set TypingSpeed 120ms

Hide
Type "unset NO_COLOR; D=$(mktemp -d); target/debug/unified-player -c $D/config -C $D/cache -o enable_audio_visualization=true demo screen home --scenario showcase --interactive"
Enter
Sleep 2.5s
Show

# 1. Home: quick access, recently played, continue listening.
Sleep 2.5s
Screenshot "docs/assets/screenshots/home.png"
Sleep 500ms

# 2. Open a playlist from Quick access.
Down@400ms
Right@400ms
Sleep 500ms
Enter
Sleep 1.6s
Screenshot "docs/assets/screenshots/playlist.png"
Sleep 500ms

# 3. Filter the playlist and play the match.
Type@200ms "/ferry"
Sleep 1.2s
Enter
Sleep 500ms
Enter
Sleep 1.5s

# 4. Queue an earlier track (the next one is already up) and look at the queue.
Escape
Sleep 300ms
Type "kZ"
Sleep 600ms
Type "z"
Sleep 1.8s

# 5. Synced lyrics with the visualizer.
Type "l"
Sleep 5s
Screenshot "docs/assets/screenshots/lyrics.png"
Sleep 500ms

# 6. Back to Home and over to YouTube Music.
Backspace@300ms 3
Sleep 800ms
Type "gm"
Sleep 2s

# 7. Preview a few dark themes live, keep one, and hold for the loop.
Type "T"
Sleep 800ms
Down@750ms 3
Sleep 300ms
Screenshot "docs/assets/screenshots/themes.png"
Sleep 600ms
Enter
Sleep 2.5s

Hide
Type "q"
