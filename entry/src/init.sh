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

# Hand off to a real init only when an init system is actually installed.
# /sbin/init (e.g. openrc) assumes a full console subsystem: the reboot
# syscall, VT ioctls, and /dev/tty1-N for getty. On a minimal rootfs without
# those, execing /sbin/init just spews errors, so keep an interactive shell.
if [ -x /sbin/init ] && command -v openrc >/dev/null 2>&1; then
    exec /sbin/init
fi

# Use bash if available, otherwise fall back to sh
if [ -x /bin/bash ]; then
    exec /bin/bash -l
else
    exec /bin/sh --login
fi
