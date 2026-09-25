# Reverse Proxy Setup - Caddy

[<= Back to Generic Deployment Guide](generic.md#setting-up-the-reverse-proxy)

We recommend Caddy as a reverse proxy, as it is trivial to use, handling TLS
certificates, reverse proxy headers, etc. transparently with proper defaults.

## Installation

Install Caddy via your preferred method. Refer to the
[official Caddy installation guide](https://caddyserver.com/docs/install) for your distribution.

## Configuration

After installing Caddy, create `/etc/caddy/conf.d/tuwunel_caddyfile` and enter this (substitute
`your.server.name` with your actual server name):

```caddyfile
your.server.name, your.server.name:8448 {
    # TCP reverse_proxy
    reverse_proxy localhost:8008
    # UNIX socket (alternative - comment out the line above and uncomment this)
    #reverse_proxy unix//run/tuwunel/tuwunel.sock
}
```

### What this does

- Handles both port 443 (HTTPS) and port 8448 (Matrix federation) automatically
- Automatically provisions and renews TLS certificates via Let's Encrypt
- Sets all necessary reverse proxy headers correctly
- Routes all traffic to Tuwunel listening on `localhost:8008`

### Client IP source

What decides this is the **peer address Tuwunel sees**, not whether Caddy runs
on the same machine. The default `localhost_ip` covers loopback only.

Nothing to configure when Caddy reaches Tuwunel over a Unix socket, or over
`127.0.0.1` on the same machine or in a shared network namespace: the peer is
loopback, so Tuwunel reads the `X-Forwarded-For` header that the
`reverse_proxy` directive already sets, and each client is counted separately.

⚠️ **Separate containers are not loopback.** In the Docker Compose example
Tuwunel sees Caddy's container address, so left alone `X-Forwarded-For` is
ignored and every request counts as Caddy: registration logs, rate limiting and
the connection limit all attribute the whole server to one address. Tuwunel
warns about this at startup. Set
`reverse_proxy_ip_header = "rightmost_x_forwarded_for"` in `tuwunel.toml` (or
`TUWUNEL_REVERSE_PROXY_IP_HEADER=rightmost_x_forwarded_for` in Docker), the same
as for a Caddy on a different host. ⚠️ The header is then believed from any
peer, so only do this when clients cannot reach Tuwunel around Caddy.

The setting only takes effect at startup, so restart Tuwunel after changing
it.

That's it! Just start and enable the service and you're set.

```bash
sudo systemctl enable --now caddy
```

## Verification

After starting Caddy, verify it's working by checking:

```bash
curl https://your.server.name/_tuwunel/server_version
curl https://your.server.name:8448/_tuwunel/server_version
```
## Caddy and .well-known

Caddy can serve `.well-known/matrix/client` and `.well-known/matrix/server`
instead of `tuwunel`. This is useful when delegating a root domain such as
`example.com` to a subdomain like `matrix.example.com`, where Caddy on the root
domain has no Tuwunel upstream of its own.

In this configuration Caddy bypasses Tuwunel's CORS layer, so the
[CORS headers recommended by the Matrix specification](https://spec.matrix.org/v1.17/client-server-api/#well-known-uris)
must be added explicitly.

> [!NOTE]
> Caddyfile uses backticks as an alternative string quote so the inline JSON
> body (which contains double quotes) does not need escaping. Field names and
> values inside a `header` block are space-separated; do not place a colon
> after the field name.

```caddyfile
example.com {
	@matrix path /.well-known/matrix/*
	header @matrix {
		Access-Control-Allow-Origin "*"
		Access-Control-Allow-Methods "GET, POST, PUT, DELETE, OPTIONS"
		Access-Control-Allow-Headers "X-Requested-With, Content-Type, Authorization"
		Content-Type "application/json"
	}
	respond /.well-known/matrix/client `{"m.homeserver":{"base_url":"https://matrix.example.com"}}`
	respond /.well-known/matrix/server `{"m.server":"matrix.example.com:443"}`
}
```

To advertise a MatrixRTC focus (MSC4143) for Element Call, extend the client
response with an `org.matrix.msc4143.rtc_foci` array pointing at your LiveKit
JWT service:

```caddyfile
respond /.well-known/matrix/client `{"m.homeserver":{"base_url":"https://matrix.example.com"},"org.matrix.msc4143.rtc_foci":[{"type":"livekit","livekit_service_url":"https://rtc.example.com"}]}`
```


---

[=> Continue with "You're Done"](generic.md#you-are-done)
