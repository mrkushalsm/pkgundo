#!/bin/sh
# %post: runs on both fresh install ($1=1) and upgrade ($1>=2). `pkgundo
# setup` is idempotent, so it's called unconditionally rather than only on
# a fresh install.
/usr/bin/pkgundo setup || true
