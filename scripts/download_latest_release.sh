#!/bin/bash
tar_gz_url=$(curl -s https://api.github.com/repos/ssalonen/webcam-proxy/releases/latest \
| grep "browser_download_url.*-linux-x86_64.tar.gz" \
| cut -d : -f 2,3 \
| tr -d \"\
| tr -d ' ')

wget "$tar_gz_url" -q -O - | tar xzf - -O > /tmp/webcam-proxy
echo "Downloaded $tar_gz_url to /tmp/webcam-proxy"