Pacman hooks
============

Some Arch Linux Pacman hooks I wrote for y own use.


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

However is written is Rust and uses a thread pool for faster processing (about ~10x faster than the above code on my machine).


### cinnamon-tweaks

Automatically patches [Cinnamon](https://github.com/linuxmint/Cinnamon) CSS at installation or upgrade to increase panel font size.


### pacdiff

Automatically run `pacdiff` after an upgrade to review pacnew files.


### reflector

Selects fastest package mirror, when the `pacman-mirrorlist` package is upgraded.
See https://wiki.archlinux.org/index.php/Reflector#Pacman_hook


### sync

Syncs `/` and `/boot` partitions when packages are installed, upgraded or removed.


## Installation



## License

[GPLv3](https://www.gnu.org/licenses/gpl-3.0-standalone.html)
