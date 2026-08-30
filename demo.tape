Output "docs/demo.gif"

Require cargo
Require docker

Set Shell "zsh"
Set FontSize 18
Set Width 1200
Set Height 760
Set Padding 16
Set TypingSpeed 35ms
Set Theme "Catppuccin Mocha"

Hide
Type "cargo build --quiet --bin sd"
Enter
Type "demo_name=super-docker-vhs-demo-$$"
Enter
Type "docker run --rm -d --name $demo_name -P nginx:alpine >/dev/null"
Enter
Type "(sleep 7; docker restart $demo_name >/dev/null) &"
Enter
Sleep 2s
Show

Type "./target/debug/sd --no-update-check"
Enter
Sleep 4s

Type "]"
Sleep 3s
Type "E"
Sleep 2s
Escape
Sleep 1s
Type "q"
Sleep 1s

Hide
Type "docker stop $demo_name >/dev/null"
Enter
