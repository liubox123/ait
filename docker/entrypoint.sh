#!/bin/sh
set -eu
mkdir -p /data
chown ait:ait /data
exec su -m -s /bin/sh ait -c 'exec ait serve --bind "$AIT_BIND" --data /data'
