#!/usr/bin/env bash
# Cross-compile the Windows exe from WSL.
#   scripts/build-win.sh            # debug, with console
#   scripts/build-win.sh --release  # no console, installed
# Other args go to cargo.
#
# --release installs to %LOCALAPPDATA%\nvright\bin (prints the path) and
# adds a Start menu shortcut. Run it from there: started from the WSL
# filesystem, Windows sometimes runs a stale cached copy.
set -euo pipefail
cd "$(dirname "$0")/.."

release=false
for arg in "$@"; do
    [[ $arg == --release ]] && release=true
done
if ! $release; then
    exec cargo xwin build --target x86_64-pc-windows-msvc "$@"
fi

# no console, but logs still show up when launched from a WSL terminal
cargo xwin build --target x86_64-pc-windows-msvc --features gui-subsystem "$@" >&2

powershell=/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe
local_appdata=$("$powershell" -NoProfile -Command '[Environment]::GetFolderPath("LocalApplicationData")' | tr -d '\r')
bin_win="$local_appdata\\nvright\\bin"
bin=$(wslpath -u "$bin_win")
mkdir -p "$bin"
if ! cp target/x86_64-pc-windows-msvc/release/nvright.exe "$bin/nvright.exe"; then
    echo "install failed: close the running nvright instances and try again" >&2
    exit 1
fi

# shortcut starts in $HOME of this distro
home_unc="\\\\wsl.localhost\\${WSL_DISTRO_NAME}$(echo "$HOME" | tr / '\\')"
"$powershell" -NoProfile -Command "
    \$lnk = Join-Path ([Environment]::GetFolderPath('Programs')) 'nvright.lnk'
    \$s = (New-Object -ComObject WScript.Shell).CreateShortcut(\$lnk)
    \$s.TargetPath = '$bin_win\\nvright.exe'
    \$s.WorkingDirectory = '$home_unc'
    \$s.Description = 'Neovim GUI'
    \$s.Save()
    Remove-Item (Join-Path ([Environment]::GetFolderPath('Programs')) 'right_editor.lnk') -ErrorAction SilentlyContinue
" >&2
echo "installed in $bin_win, with a Start menu shortcut" >&2
echo "$bin/nvright.exe"
