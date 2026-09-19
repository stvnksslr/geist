# giest: WSL shell-integration bootstrap (POSIX sh, not part of upstream Ghostty).
#
# giest launches `wsl.exe -e /bin/sh -c 'exec /bin/sh "$GIEST_SHELL_INTEGRATION_DIR/giest-wsl.sh"'`
# with GIEST_SHELL_INTEGRATION_DIR translated to a Linux path by WSLENV's `/p`
# flag. This script finds the user's login shell and injects Ghostty's own
# integration scripts (vendored next to this file) with the same per-shell
# mechanism as Ghostty's src/termio/shell_integration.zig:
#
#   bash     ENV=<dir>/bash/ghostty.bash + `bash --posix` (ghostty.bash then
#            leaves POSIX mode and sources the normal startup files itself)
#   zsh      ZDOTDIR=<dir>/zsh (its .zshenv restores the user's ZDOTDIR)
#   fish     XDG_DATA_DIRS=<dir>:... (fish/vendor_conf.d)
#   elvish   XDG_DATA_DIRS=<dir>:... (elvish/lib)
#   nushell  XDG_DATA_DIRS=<dir>:... + `--execute 'use ghostty *'`
#
# Anything unrecognised just runs the login shell untouched.

dir=$GIEST_SHELL_INTEGRATION_DIR
kind=${GIEST_SHELL_INTEGRATION:-detect}
unset GIEST_SHELL_INTEGRATION_DIR GIEST_SHELL_INTEGRATION

shell=${SHELL:-}
if [ -z "$shell" ]; then
  shell=$(getent passwd "$(id -un)" 2>/dev/null | cut -d: -f7)
fi
[ -n "$shell" ] || shell=/bin/sh

if [ "$kind" = detect ]; then
  case ${shell##*/} in
    bash) kind=bash ;;
    zsh) kind=zsh ;;
    fish) kind=fish ;;
    elvish) kind=elvish ;;
    nu) kind=nushell ;;
    *) kind=none ;;
  esac
fi

if [ -z "$dir" ] || [ ! -d "$dir" ]; then
  kind=none
fi

xdg() {
  export GHOSTTY_SHELL_INTEGRATION_XDG_DIR="$dir"
  export XDG_DATA_DIRS="$dir:${XDG_DATA_DIRS:-/usr/local/share:/usr/share}"
}

case $kind in
  bash)
    if [ -n "${ENV+x}" ]; then export GHOSTTY_BASH_ENV="$ENV"; fi
    export ENV="$dir/bash/ghostty.bash"
    export GHOSTTY_BASH_INJECT=1
    # POSIX mode defaults HISTFILE to ~/.sh_history; keep ~/.bash_history.
    if [ -z "${HISTFILE+x}" ]; then
      export HISTFILE="$HOME/.bash_history"
      export GHOSTTY_BASH_UNEXPORT_HISTFILE=1
    fi
    exec "$shell" --posix -l
    ;;
  zsh)
    if [ -n "${ZDOTDIR+x}" ]; then export GHOSTTY_ZSH_ZDOTDIR="$ZDOTDIR"; fi
    export ZDOTDIR="$dir/zsh"
    exec "$shell" -l
    ;;
  fish)
    xdg
    exec "$shell" -l
    ;;
  elvish)
    xdg
    exec "$shell"
    ;;
  nushell)
    xdg
    exec "$shell" -l --execute 'use ghostty *'
    ;;
esac

exec "$shell" -l
