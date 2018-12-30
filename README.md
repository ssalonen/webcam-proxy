# Proxy to web camera

## Introduction

Sometimes you have web camera behind poor or unreliable network connection. In
many cases the web camera itself is quite low-performance and might struggle to
serve the video feed or image snapshot in a performant manner.

`webcam-proxy` is to help in these cases. It acts as reverse proxy for the web
camera, caching last image. The application is optimized to serve multiple
clients. Both mjpeg and jpeg endpoints are provided.

This is particularly useful when the proxy is behind a good connection, but the
web camera is behind low-bandwidth connection.

## Configuration

The application takes config file as the argument:

```bash
cargo run config.toml
```

where `config.toml` is something like

```toml
[server]
listen = "127.0.0.1:3000"

[server.auth]
admin = "admin"
user2 = "userpass"

[webcam]
# Example of proxying FOSCAM camera feed
# url = "https://MYWEBCAM/cgi-bin/CGIProxy.fcgi?cmd=snapPicture2&usr=MYUSER&pwd=MYPASSWORD"
url = "https://www.portofhelsinki.fi/webcams/image_00001.jpg?1550313659718"
```

You can then access the mjpeg feed at
http://localhost:3000/stream?usr=admin&pwd=admin and the jpeg at
http://localhost:3000/snapshot?usr=admin&pwd=admin.

## nginx configuration

The repository contains `nginx.conf` configuration example to reverse proxy
`cmd=snapPicture2` and `cmd=GetMJStream` to this service, while other endpoints
are accessing the backing web camera directly.

By pointing your web camera client to this nginx, you can enjoy the benefits
of fast mjpeg and cached jpeg responses, while having access to other functions
of FOSCAM web camera (e.g. admin interface).

## systemd service setup

`example.service` is shows how to setup

## Features

- stale image detection while showing last image capture
- simple authentication, similar to FOSCAM API using `pwd` and `usr` query string parameters

## Other

`/ci` from https://github.com/japaric/trust
