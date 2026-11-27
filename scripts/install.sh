#!/bin/sh
# Build the plugin host and install it as `lvim-plugin` (the program the lvim tools and the Neovim plugins start for the plugins).
exec sh "$(dirname "$0")/bootstrap.sh" host
