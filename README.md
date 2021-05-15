Pacman hooks
============

[![Build status](https://img.shields.io/github/workflow/status/desbma/pacman-hooks/check-broken-packages.svg?style=flat)](https://github.com/desbma/pacman-hooks/actions/)
[![AUR version](https://img.shields.io/aur/version/check-broken-packages-git.svg?style=flat)](https://aur.archlinux.org/packages/check-broken-packages-pacman-hook-git/)
[![License](https://img.shields.io/github/license/desbma/pacman-hooks.svg?style=flat)](https://github.com/desbma/pacman-hooks/blob/master/LICENSE)

Some useful Arch Linux Pacman hooks.


## Hooks

### check-broken-packages

This checks for packages with broken (non satisfied) dynamic library dependencies.
This can happen if you have installled package *A* from the AUR, which depends on *B* from the official repositories, and *B* gets updated, but the packager of *A* does not bump its [`pkgrel`](https://wiki.archlinux.org/index.php/PKGBUILD#pkgrel). In most case you simply need to rebuild *A*.

This is roughly equivalent to the following Bash code:

    IFS='
    '
    for aur_package in $(pacman -Qmm | cut -d ' ' -f 1); do
      for package_file in $(pacman -Ql ${aur_package} | cut -d ' ' -f 2); do
        if [ -f ${package_file} -a -x ${package_file} ]; then
          ldd_output=$(ldd ${package_file} 2> /dev/null)
          if [ $? -eq 0 ]; then
            for line in $(echo ${ldd_output} | grep -F '=> not found'); do
              echo "Missing depency for file ${package_file} from package ${aur_package}: ${line}"
            done
          fi
        fi
      done
    done

However it is written in Rust and uses a thread pool for **much faster** processing (runs in ~1.3s on my machine with ~90 AUR packages, compared to ~14s for the above Bash code).

The hook also detects:

* broken Python packages that were build for an older Python major version
* broken Systemd links for enabled services in `/etc/systemd/{user,system}/*.target.*`.


### pacdiff

Automatically run `pacdiff` after an upgrade to review pacnew files.


### reflector

Selects fastest package mirror, when the `pacman-mirrorlist` package is upgraded.
See https://wiki.archlinux.org/index.php/Reflector#Pacman_hook


### sync

Syncs `/` and `/boot` partitions when packages are installed, upgraded or removed.


### xmonad-recompile

Automatically run `xmonad --recompile` for each user in the system after `xmonad` or any of its dependencies is updated.


## Installation

Install via the AUR packages:

* [check-broken-packages-pacman-hook-git](https://aur.archlinux.org/packages/check-broken-packages-pacman-hook-git/)
* [pacdiff-pacman-hook-git](https://aur.archlinux.org/packages/pacdiff-pacman-hook-git/)
* [reflector-pacman-hook-git](https://aur.archlinux.org/packages/reflector-pacman-hook-git/)
* [sync-pacman-hook-git](https://aur.archlinux.org/packages/sync-pacman-hook-git/)
* [xmonad-recompile-pacman-hook-git](https://aur.archlinux.org/packages/xmonad-recompile-pacman-hook-git/)

This was previously contained in a single package `pacman-hooks-desbma-git`, however this is against the AUR guidelines so each hook is now available in a separate package.

## License

[GPLv3](https://www.gnu.org/licenses/gpl-3.0-standalone.html)
