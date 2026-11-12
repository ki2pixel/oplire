```
   ___      ___    _       ___     ___     ___
  / _ \    | _ \  | |     |_ _|   | _ \   | __|
 | (_) |   |  _/  | |__    | |    |   /   | _|
  \___/   _|_|_   |____|  |___|   |_|_\   |___|
 _|"""""|_| """ |_|"""""|_|"""""|_|"""""|_|"""""|
 "`-0-0-'"`-0-0-'"`-0-0-'"`-0-0-'"`-0-0-'"`-0-0-'
     OpenCode Proxy Pool Manager
     by Berke Oruc
```

[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange?style=flat-square&logo=rust)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue?style=flat-square)](LICENSE)
[![Version](https://img.shields.io/badge/version-3.0.0-green?style=flat-square)](Cargo.toml)

## What is oplire?

**oplire** is a proxy pool manager and rate-limit rotation tool designed for **OpenCode** (CLI & Desktop):

1. **Residential & ISP Proxy Pool Manager**: Provides local HTTP CONNECT forward tunneling with upstream Basic Auth forwarding and automatic rotation across high-quality Static Residential ISP proxies (Proxiware, Webshare, etc.). Bypasses daily per-IP quota limits on heavy free models (like `opencode/deepseek-v4.1-flash-free`).
2. **Log-Based Rate-Limit Rotation**: Watches OpenCode logs in real time and rotates your outbound connection when a quota or 429 error is detected, with zero process restarts.
3. **Forensic Audit & Pool Sizing**: Audits OpenCode conversation transcripts to pinpoint exact rate-limit cutoff boundaries, analyze token consumption, and accurately size your proxy pool (`oplire audit` / `oplire estimate`).

---

### How It Works

#### 1. Residential & ISP Forward Tunneling (OpenCode Desktop & CLI)
OpenCode imposes daily message quotas per IP on free models (e.g. DeepSeek V4.1 Flash, GLM 5.3 Flash). Datacenter and shared IPs are often exhausted quickly.
- `oplire proxy` opens a local forward proxy on `127.0.0.1:8080` (configurable with `--listen` or persistent config).
- OpenCode Desktop and CLI route egress HTTPS traffic through `oplire` via standard `HTTPS_PROXY`, `HTTP_PROXY`, and `ALL_PROXY` variables configured via `eval $(oplire env)`.
- `oplire` injects upstream Basic Auth credentials, tunnels TLS directly to target APIs, and monitors connection status.
- When an IP quota or rate limit is reached, `oplire` automatically rotates to the next proxy in your pool and immediately terminates active CONNECT tunnels so client sessions re-establish through the new outbound proxy with zero process restarts.

#### 2. Log-Based Real-time Watch Mode
- Auto-detects active OpenCode logs across all supported operating systems:
  - **Linux**: `~/.local/share/opencode/log/opencode.log` or `$XDG_DATA_HOME/opencode/log/opencode.log`
  - **macOS**: `~/Library/Application Support/opencode/log/opencode.log`
  - **Windows**: `%LOCALAPPDATA%\opencode\log\opencode.log`, `%APPDATA%\opencode\log\opencode.log`, or `%USERPROFILE%\AppData\...\opencode\log\opencode.log`
  - **Custom**: user-specified log path via `--log-file <path>`
- Watches appended log entries in real time for 429 rate-limit errors or quota warnings.
- Rotates to the next proxy in the pool automatically with a configurable debounce delay (`--delay-ms`, default 5000ms).
- Falls back to periodic HTTP polling on the upstream `/v1/models` endpoint when no log file is found or the log cannot be opened initially.

#### 3. Empirical Free Quota Thresholds & Capacity Law
Forensic and quantitative analysis of OpenCode free model transcripts (such as `opencode/deepseek-v4.1-flash-free`) proves that rate limits (`FreeUsageLimitError` / HTTP 429) are strictly governed by **output completion tokens**:
- **Output Token Quota per IP**: Capped at **~60,000 to 65,000 output tokens** per residential/ISP IP (measured average: ~62,200 tokens, standard deviation: $\pm 3.4\%$).
- **Independence from Other Metrics**: Total input tokens (150k to 352k), prompt cache reads (27M to 42M tokens), reasoning tokens (54k to 88k), and assistant message counts (149 to 247 requests) do **not** trigger early rate-limiting.
- **Recharge Sliding Window**: Exhausted IPs require a cooldown period of **~8 to 9 hours** before quotas recharge (measured from upstream `retryAfterMs` spanning ~29.1M to ~33.1M ms).
- **Pool Sizing Law**:
  $$N_{\text{proxies}} = \left\lceil \frac{T_{\text{output\_workload}}}{60\,000} \right\rceil$$
  Recommended pool size includes a +1 operational safety reserve:
  $$N_{\text{recommended}} = N_{\text{proxies}} + 1$$

### Security Notes

- **SSRF protection**: `CONNECT` targets that resolve to loopback, link-local, RFC 1918, CGNAT (`100.64.0.0/10`), or reserved `.local` / `.internal` / `.localhost` names are rejected with `403 Forbidden`. Hostnames are resolved with a 3-second bound to mitigate DNS rebinding.
- **DoS limits**: at most 512 concurrent client connections; client and upstream headers are read under 5 s / 8 s timeouts and capped at 8 KiB / 4 KiB respectively.
- **File permissions (Unix)**: `~/.config/oplire` is created with `0700`; `proxies.txt` and `config.json` are written with `0600`.
- **Binding warning**: binding to a non-loopback address logs a security warning because the local proxy has no inbound authentication.
- **`/health`**: `GET` returns `OK`; `HEAD` returns the same headers (`Content-Length: 2`) without any body bytes, per RFC 9110.

---

## Installation

### From Source (Recommended)
```bash
git clone https://github.com/BerkeOruc/oplire.git
cd oplire
cargo build --release
cp target/release/oplire ~/.local/bin/oplire
# Or system-wide:
# sudo cp target/release/oplire /usr/local/bin/oplire
```

### Linux (AUR)
```bash
yay -S oplire
```

### Windows (winget)
```powershell
winget install BerkeOruc.oplire
```

### macOS (Homebrew)
```bash
brew install berkeoruc/oplire/oplire
```

---

## Quick Start

### Configure your Proxy Pool

Add your Static Residential / ISP proxies (e.g., from [Proxiware](https://proxiware.com)):
```bash
# Option 1: Import a text file (host:port:user:pass)
oplire proxies import /path/to/my-proxies.txt

# Option 2: Fetch directly via Proxiware API
oplire proxies fetch --api-key "YOUR_PROXIWARE_API_KEY"
# Or export PROXIWARE_API_KEY in your environment:
# export PROXIWARE_API_KEY="YOUR_PROXIWARE_API_KEY"
# oplire proxies fetch

# Option 3: Edit ~/.config/oplire/proxies.txt directly
```

Verify your proxy pool:
```bash
oplire proxies list
oplire proxies test
```

### Start the Proxy

In a terminal window:
```bash
oplire proxy
```
> [!NOTE]
> `oplire proxy` automatically launches the real-time OpenCode log watcher in the background alongside the HTTP forward proxy server.

### Run OpenCode

In another terminal session:
```bash
eval $(oplire env)
opencode
```
Or run OpenCode CLI directly:
```bash
eval $(oplire env)
opencode run -m opencode/deepseek-v4.1-flash-free "Hello from OpenCode!"
```

> [!NOTE]
> `oplire env` automatically exports `NO_PROXY="localhost,127.0.0.1,::1"`. This is crucial so OpenCode's internal stdio/IPC and local server communication bypasses the proxy.

> [!IMPORTANT]
> **OpenCode Desktop Background Daemon**:
> OpenCode Desktop spawns a background service (`opencode-cli serve --service`) that stays alive even when the GUI window is closed. If OpenCode was previously started directly (without proxy variables, or following an in-app update), new windows will quietly reconnect to that non-proxied daemon, bypassing the proxy pool.
> To terminate any orphaned background daemon and force a clean reload with proxy:
> ```bash
> pkill -f "opencode-cli serve"
> ```

---

## Command Reference

### Proxy & Pool Management
```bash
oplire proxy                          # Start forward proxy on 127.0.0.1:8080 (includes background log watcher)
oplire proxy --listen 127.0.0.1:9000  # Start on custom listen address
oplire proxy --proxy-file <file>      # Start using an explicit proxy pool file
oplire daemon                         # Run proxy and watcher as a background daemon
oplire daemon --listen 127.0.0.1:9000 # Run daemon on custom address
oplire daemon --proxy-file <file>     # Run daemon with custom proxy file
oplire watch                          # Standalone OpenCode log monitor & auto-rotation
oplire watch --delay-ms 5000          # Set debounce delay after a rotation (default: 5000ms)
oplire watch --log-file <path>        # Explicit OpenCode log file path
oplire watch --upstream <url>         # Fallback upstream URL for HTTP polling (default: http://localhost:3000)
oplire watch --proxy-file <file>      # Custom proxy pool file to rotate
oplire proxies list                   # List all proxies and highlight the active one (★)
oplire proxies test                   # Test ping latency and public exit IP for all pool proxies
oplire proxies next                   # Manually rotate to the next proxy in the pool
oplire proxies import <file>          # Import proxy list into ~/.config/oplire/proxies.txt
oplire proxies fetch --api-key <key>  # Fetch static ISP proxies from Proxiware API
oplire env                            # Print shell export statements for 127.0.0.1:8080
oplire env --listen 127.0.0.1:9000    # Print shell export statements for custom listen address
```

### Installation, Diagnostics & Configuration
```bash
oplire install opencode               # Install OpenCode CLI/desktop dependencies via npm
oplire setup                          # Interactive step-by-step setup wizard
oplire doctor                         # Diagnose OpenCode, upstream reachability, and network ports
oplire models                         # List available upstream OpenCode models (default: http://localhost:3000)
oplire models --upstream <url>        # List models from a custom OpenCode Zen upstream URL
oplire config show                    # Show current configuration values and config file path
oplire config set --listen :8080      # Set default listen address (:8080 -> 127.0.0.1:8080)
oplire config set --upstream <url>    # Set default OpenCode Zen upstream URL
oplire config reset                   # Reset configuration to defaults
oplire about                          # Show version, author, and system information
```

### Forensic Audit & Pool Sizing
```bash
oplire audit <transcript.json>        # Forensic analysis of OpenCode transcript (rotations, tokens, quota left)
oplire audit <transcript.json> --format json # Output raw audit report in JSON for scripting/CI
oplire estimate -o <tokens_out>       # Sizing recommendation based on target output tokens
oplire estimate -o 150000 -i 500000   # Sizing recommendation with projected prompt input tokens
oplire estimate -o 150000 -q 60000    # Estimate with custom quota threshold
oplire estimate -o 150000 --format json # Output estimation in JSON
```

> [!TIP]
> **Observability**: Run any command with `-v` / `--verbose` or set `RUST_LOG=debug` (e.g. `RUST_LOG=debug oplire proxy`) to enable detailed tracing logs.

---

## Proxy Configuration Details

### Proxy Pool File Discovery Order
When starting `oplire proxy`, `oplire watch`, or `oplire daemon`, proxies are loaded following this priority:
1. `~/.config/oplire/proxies.txt` (or `$XDG_CONFIG_HOME/oplire/proxies.txt`)
2. `proxiware-isp.txt` in the current working directory (automatically imported to configuration)
3. `~/oplire-master/proxiware-isp.txt` (fallback, automatically imported to configuration)
4. `Webshare-10-proxies.txt` in the current working directory
5. Custom file path specified via `--proxy-file <path>`

### Persistence & Hot-Reloading
- **Active Proxy Index**: Persistent rotation state is saved in `~/.config/oplire/active_proxy.idx`.
- **Dynamic Hot-Reload**: oplire monitors file modification timestamps (`mtime`) on both `proxies.txt` and `active_proxy.idx`. If you edit `proxies.txt` or rotate proxies in another terminal via `oplire proxies next`, the running proxy server immediately detects the change without requiring a restart. Filesystem checks are throttled to at most once per second so a `CONNECT` never triggers synchronous disk I/O.
- **Active Tunnel Teardown**: Upon rotation, oplire broadcasts an abort signal across all active CONNECT tunnels, triggering an immediate reconnect through the newly selected proxy.

### Supported Formats in `proxies.txt`
```text
# Standard host:port:username:password (with optional label/tag)
188.220.163.167:1337:username:password # proxiware-isp
51.241.234.51:1337:username:password

# Passwords may contain ':' (splitn(4) parsing)
188.220.163.167:1337:username:p@ss:w0rd:123 # proxiware-isp

# URL format (only http:// is supported for CONNECT tunneling)
http://username:password@188.220.163.167:1337
# socks5:// and https:// entries are parsed but rejected with 502 Bad Gateway

# IP:Port without auth (if exit IP is whitelisted on provider dashboard)
188.220.163.167:1337
```

### Provider Auto-Detection
If no explicit tag (e.g. `# my-provider`) is provided:
- Port `1337` or IPs starting with `188.220.` / `51.241.` or containing `proxiware` are classified as `proxiware-isp`.
- Port `8000` or hosts containing `webshare` are classified as `webshare`.
- Other entries default to `custom`.

---

## Available Free Models

Model IDs available on OpenCode Zen:

| Model | ID |
|---|---|
| DeepSeek V4.1 Flash Free | `opencode/deepseek-v4.1-flash-free` |
| GLM 5.3 Flash Free | `opencode/glm-5.3-flash-free` |
| GLM 4.7 Free | `glm-4.7-free` |
| MiniMax M2.1 Free | `minimax-m2.1-free` |
| Kimi K2.5 Free | `kimi-k2.5-free` |
| Qwen 2.5 72B Free | `qwen-2.5-72b-free` |
| Llama 3.3 70B Free | `llama-3.3-70b-free` |

---

## Troubleshooting & Gotchas

### 1. OpenCode Desktop Bypasses Proxy or Reports Quota Exceeded on Direct IP
OpenCode Desktop relies on a background server (`opencode-cli serve --service`). If OpenCode was launched without proxy environment variables (e.g. after a desktop update or clicking the default icon), this daemon persists silently even after closing the window. Subsequent launches will reconnect to that unproxied daemon and exhaust your direct IP quota.

To resolve:
```bash
# 1. Kill any stale background service running without proxy
pkill -f "opencode-cli serve"

# 2. Restart OpenCode with proxy environment variables
eval $(oplire env)
opencode &
```

> [!TIP]
> Always open a **new chat session** if a previous conversation remains blocked in a cached `QuotaExceeded` error state.

### 2. Quota Reset Schedule
OpenCode calculates daily free message quotas strictly by IP address, resetting at **00:00:00 UTC** daily:
- **02:00 CEST** (UTC+2 / Summer time in Western Europe)
- **01:00 CET** (UTC+1 / Winter time in Western Europe)

---

## Upgrading from v2

oplire 3.0.0 removes the Claude Code integration and Cloudflare WARP support:

- The Anthropic ↔ OpenCode bridge and its reverse endpoints (`/v1/messages`, `/v1/models`, `/v1/chat/completions`) are gone. `oplire proxy` is now a forward HTTP CONNECT proxy only. The `/health` endpoint remains available (`GET` and `HEAD`).
- Removed commands: `status`, `reset`, `quick-reset`, `stop`, `connect claude-code`, `install warp`, `install claude-code`, `install all`.
- Removed flags: `--dry-run`, `--api-key`, `--max-retries`, `--warp-delay`.
- The configuration keys `api_key`, `max_retries`, and `warp_delay` are ignored. Run `oplire config reset` or re-save the configuration to clean them up.
- Rate-limit rotation continues through the proxy pool: `oplire watch` (started automatically by `oplire proxy` and `oplire daemon`) rotates to the next proxy when OpenCode reports a limit.

---

## License

MIT License. See [LICENSE](LICENSE) for details.
