Output "docs/demo.gif"

Require cargo
Require docker

Set Shell "bash"
Set FontSize 18
Set Width 1200
Set Height 760
Set Padding 16
Set TypingSpeed 5ms
Set Theme { "background": "#080c10", "foreground": "#eef0ee", "cursor": "#f5ac85", "selection": "#302724" }

Hide
Type "unset NO_COLOR"
Enter
Type "export COLORTERM=truecolor"
Enter
Type "cargo build --quiet --bin sd"
Enter
Type "demo_name=super-docker-vhs-demo-$$"
Enter
Type "demo_db=$(mktemp -t super-docker-vhs-history)"
Enter
Type "export SUPER_DOCKER_DB=$demo_db SUPER_DOCKER_NO_UPDATE_CHECK=1"
Enter
Type "trap 'docker rm -f $demo_name >/dev/null 2>&1; rm -f $demo_db $demo_db-wal $demo_db-shm' EXIT"
Enter
Type "docker run --rm -d --name $demo_name -P nginx:alpine >/dev/null"
Enter
Type "(sleep 12; docker restart $demo_name >/dev/null) &"
Enter
Sleep 2s

Type "./target/debug/sd --no-update-check"
Enter
Sleep 2s
Type "/super-docker-vhs-demo"
Enter
Sleep 1s
Show
Screenshot docs/demo.png
Sleep 4s

Type "]"
Sleep 3s
Type "E"
Sleep 2s
Escape
Sleep 1s
Hide
Type "q"
Sleep 1s

Type "docker stop $demo_name >/dev/null"
Enter
Type "rm -f $demo_db $demo_db-wal $demo_db-shm"
Enter
