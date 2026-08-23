#!/bin/sh
# %preun: $1=0 means this is the final removal (no version remains
# installed); $1>=1 means an upgrade is in progress and a new version's
# %post will re-run `pkgundo setup` anyway, so only tear down on a genuine
# uninstall.
if [ "$1" = "0" ]; then
    /usr/bin/pkgundo setup --remove || true
fi
