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

# Use bash if available, otherwise fall back to sh
if [ -x /bin/bash ]; then
    exec /bin/bash -l
else
    exec /bin/sh --login
fi
