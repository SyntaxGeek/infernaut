# Running Infernaut in Docker

The ARM64 image has been smoke-tested on Docker Desktop for macOS, including
authentication, inference, TUI save/restart and graceful draining. The synthetic
configuration in `tests/docker-smoke.toml` targets `tests/mock_provider.py` on
the host's loopback port 18080 through Docker Desktop's `host.docker.internal`.
Use a disposable writable config directory and a random loopback port for tests;
do not run the fixture against a production gateway. Docker may assign a new
random port after restart; query `docker port CONTAINER 4040` again.

Create a `config` directory beside `compose.yaml`. Copy `infernaut.example.toml`
to `config/infernaut.toml` and your credentials to `config/.env`. Set
`server.listen = "0.0.0.0:4040"` inside that TOML. Configure provider URLs using
addresses reachable from the container, not the container's `localhost`.

The container runs as UID 10001. On Linux, give that UID write access to the
configuration directory and files; the TUI needs directory access for atomic
saves and backups. Do not mount only the TOML file. Keep authentication enabled.

```sh
docker compose up -d --build
docker compose logs -f infernaut
docker compose exec -e TERM=xterm-256color infernaut infernaut configure /config/infernaut.toml
docker compose restart infernaut
docker compose exec -e TERM=xterm-256color infernaut infernaut observe /config/infernaut.toml --url http://127.0.0.1:4040
```

Publication binds to localhost by default. For LAN clients, set
`INFERNAUT_BIND_IP` in the shell before `docker compose up -d`, using the host's
LAN IP. Use a TLS reverse proxy when traffic crosses an untrusted network.

The executable loads `/config/.env` on startup. Changing it requires a restart;
changing Compose environment or port mappings requires `docker compose up -d`
to recreate the container. OAuth credentials persist in the named state volume.
Do not use `docker compose down -v` unless you intend to delete those credentials.

SIGINT/SIGTERM stops new connections and drains existing connections for up to
900 seconds. `INFERNAUT_SHUTDOWN_GRACE_SECONDS` overrides this deadline. Keep
Compose's `stop_grace_period` longer than the gateway deadline. Disconnect the
observer before planned shutdown, since its event stream is long-lived.

Provider response headers have a 900-second deadline, adjustable with
`INFERNAUT_UPSTREAM_HEADER_TIMEOUT_SECONDS`. This is not a total generation
deadline. Request limits are 16 MiB for JSON and 100 MiB for transcription
uploads; buffered responses and SSE parsing buffers are limited to 32 MiB.
Provider redirects are rejected; configure the final upstream URL directly.

Optional Mission Control audio requires host playback tools and is not included
in this headless image. Run the observer on your host for sound.
