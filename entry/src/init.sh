#!/bin/sh

export HOME=/root
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

printf "Welcome to \e[96m\e[1mKylin X\e[0m!\n"
env
echo

printf 'Use \033[1m\033[3mapt\033[0m to install packages.\n'
echo

# Do your initialization here!

cd ~

# OpenRC's init does not consume /etc/inittab, so an installed openrc binary
# is the compatibility signal for an OpenRC rootfs. Hand off for it even when
# inittab is absent. BusyBox init, by contrast, trusts /etc/inittab without
# checking that its commands exist. A stock Alpine inittab still names OpenRC
# in sysinit when the package is absent; BusyBox then leaves getty on tty1-6
# and never exposes the serial console. Hand off for OpenRC, or for a
# BusyBox-style inittab when at least one sysinit entry exists and every
# sysinit command is executable. BusyBox init must be PID 1, so this has to be
# exec, not a background child.

# BusyBox init resolves a command without a slash through PATH. Do the same
# instead of testing the current directory or accepting a shell builtin that
# init cannot exec.
init_command_is_executable() {
    [ -n "$1" ] || return 1
    case $1 in
        */*) [ -f "$1" ] && [ -x "$1" ] ;;
        *)
            old_ifs=$IFS
            IFS=:
            found_cmd=0
            for dir in $PATH; do
                [ -n "$dir" ] || dir=.
                if [ -f "$dir/$1" ] && [ -x "$dir/$1" ]; then
                    found_cmd=1
                    break
                fi
            done
            IFS=$old_ifs
            [ "$found_cmd" -eq 1 ]
            ;;
    esac
}

inittab_sysinit_runnable() {
    found=0
    while IFS= read -r line; do
        # BusyBox ignores blank lines and comments. Strip leading whitespace
        # before deciding whether the line starts with '#'.
        line=${line#"${line%%[![:space:]]*}"}
        case $line in
            '' | '#'*) continue ;;
        esac
        # inittab fields are id:runlevels:action:process. The id is often
        # empty, so do not use field splitting to reject blank records.
        rest=${line#*:}
        rest=${rest#*:}
        action=${rest%%:*}
        process=${rest#*:}
        [ "$action" = sysinit ] || continue
        found=1
        cmd=${process%% *}
        init_command_is_executable "${cmd#-}" || return 1
    done < /etc/inittab
    [ "$found" -eq 1 ]
}

# systemd rootfs (e.g. Arch-based ztOS): hand off to systemd as PID 1 only
# when it is the configured init, i.e. /sbin/init resolves to the systemd
# binary. Merely having systemd installed must not override a rootfs that
# configured OpenRC/sysvinit (the /sbin/init branch below handles those).
# systemd mounts /proc, /sys, /dev and sets up cgroups itself.
if [ -x /lib/systemd/systemd ] && [ -x /sbin/init ] &&
    [ "$(readlink -f /sbin/init)" = "$(readlink -f /lib/systemd/systemd)" ]; then
    exec /lib/systemd/systemd
fi

if [ -x /sbin/init ] && {
    command -v openrc >/dev/null 2>&1 ||
        { [ -r /etc/inittab ] && inittab_sysinit_runnable; }
}; then
    exec /sbin/init
fi

# PID 1 inherited console file descriptions opened by the kernel bootstrap,
# which intentionally did not claim a controlling terminal. Reopen the console
# from user context so the session leader acquires it before starting an
# interactive fallback shell. The init handoff above remains free for getty.
# Keep all three descriptors readable and writable: pagers read keys from
# stderr when stdin is a pipe.
exec 0<>/dev/console 1>&0 2>&0

# Use bash if available, otherwise fall back to sh
if [ -x /bin/bash ]; then
    exec /bin/bash -l
else
    exec /bin/sh --login
fi
