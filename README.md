# Broadside

One-way ActivityPub server for organizations. Publish to the fediverse without running Mastodon.

Broadside gives your organization a federated presence (`@announcements@corp.example`) that people can follow from Mastodon, Misskey, GoToSocial, or any ActivityPub client. You publish via CLI, RSS feed, webhook, or file drop. Broadside federates the content. There is no client API, no user login, no timeline to read.

Tested and working against mastodon.social.

## Quick start

```bash
broadside init /var/lib/broadside
broadside persona add announcements --display-name="ACME Announcements"
broadside persona update announcements --bio="Official announcements" \
  --field "Website=https://acme.example"
broadside serve
```

Point your reverse proxy at `127.0.0.1:3000` and you're live.

## Posting

```bash
# Plain text
broadside post --persona=announcements "We shipped v2.0 today."

# Markdown from stdin
broadside post --persona=announcements --markdown < release-notes.md

# With image attachments
broadside post --persona=announcements --media=screenshot.png "New UI landed"

# Or configure automatic ingestion in config.toml:
# - RSS/Atom feed polling
# - Webhook endpoint (POST /hook/{persona})
# - Directory watcher (drop a .md file, it gets federated)
```

Hashtags (`#release`), mentions (`@user@instance`), and URLs are automatically linked and rendered correctly in Mastodon.

## What it does

- Federates posts to followers via ActivityPub (Create{Note})
- Accepts follows automatically (Follow → Accept)
- Serves actor profiles with content negotiation (JSON-LD for AP clients, HTML for browsers)
- WebFinger, NodeInfo, outbox, followers collection
- HTTP signature verification on all inbound inbox requests
- Multiple personas per domain (`@engineering@`, `@blog@`, `@releases@`)
- Media attachments with MIME validation, EXIF stripping, resize, and blurhash
- Profile metadata fields (Website, GitHub, etc. — rendered on Mastodon profiles)
- Brand theming via W3C Design Tokens (light/dark mode) or custom CSS
- Delivery retry with exponential backoff and per-domain circuit breaker
- CDN-friendly caching (`Cache-Control`, `Vary: Accept` on content-negotiated endpoints)
- Rate limiting, SSRF protection, Digest/Date verification
- Graceful shutdown on SIGTERM/SIGINT

## What it doesn't do

- No Mastodon Client API. No OAuth. No web UI for posting.
- No inbound content processing. Replies, likes, and boosts are accepted and discarded.
- No timelines, notifications, or streaming.
- No moderation tools. There is no community to moderate.

## Four ways to post

| Method | How | When to use |
|---|---|---|
| **CLI** | `broadside post --persona=NAME "content"` | Manual posts, scripting, cron |
| **RSS/Atom** | Configure feed URL in `config.toml` | Auto-federate blog posts |
| **Webhook** | `POST /hook/{persona}` with `Authorization: Bearer KEY` | CI/CD, CMS, internal tools |
| **Directory watch** | Drop a `.md` file in a watched directory | Simple file-based workflow |

## Configuration

Single file: `config.toml`.

```toml
[server]
bind = "127.0.0.1:3000"
domain = "corp.example"
data_dir = "/var/lib/broadside"

# Auto-federate your blog's RSS feed
[[feed]]
persona = "blog"
url = "https://corp.example/blog/feed.xml"
poll_interval = "15m"

# Accept posts from CI/CD via webhook
[[webhook]]
persona = "releases"
key = "change-this-to-a-real-secret"

# Watch a directory for markdown files
[watch]
persona = "announcements"
path = "/var/spool/broadside/incoming/"
published = "/var/spool/broadside/published/"
pattern = "*.md"
```

## Brand theming

Profile pages match your brand out of the box. Two options, composable:

**W3C Design Tokens** — hand broadside a standard [design tokens](https://tr.designtokens.org/format/) JSON file exported from Figma, Style Dictionary, or any tokens tool. Broadside maps six named tokens to its UI, with automatic light/dark mode support.

```toml
[server]
theme_tokens_path = "/var/lib/broadside/brand-tokens.json"
```

```json
{
  "color": {
    "primary":    { "$value": "#0052CC" },
    "background": { "$value": "#FFFFFF" },
    "surface":    { "$value": "#F4F5F7" },
    "text":       { "$value": "#172B4D" },
    "muted":      { "$value": "#6B778C" },
    "border":     { "$value": "#DFE1E6" }
  },
  "color-dark": {
    "primary":    { "$value": "#4C9AFF" },
    "background": { "$value": "#1B2638" },
    "surface":    { "$value": "#253858" },
    "text":       { "$value": "#E6EDFA" },
    "muted":      { "$value": "#8993A4" },
    "border":     { "$value": "#344563" }
  }
}
```

**Custom CSS** — for anything beyond colors (fonts, layout, logo), point to a CSS file. It layers on top of the design tokens.

```toml
[server]
custom_css_path = "/var/lib/broadside/brand.css"
```

Both paths are optional. Without them, broadside uses a clean default with automatic dark mode.

## Deployment

### Docker (recommended)

```bash
docker pull fallenpegasus/broadside:latest
docker run -d --name broadside \
  -v broadside-data:/data \
  -p 3000:3000 \
  fallenpegasus/broadside
```

Initialize and create a persona:

```bash
docker exec broadside broadside init /data
docker exec broadside broadside persona add announcements \
  --display-name="ACME Announcements"
```

Images are published automatically on every release to [Docker Hub](https://hub.docker.com/r/fallenpegasus/broadside). Tags: `latest`, plus version numbers (e.g. `0.3.1`).

### Docker Compose

```yaml
services:
  broadside:
    image: fallenpegasus/broadside:latest
    restart: unless-stopped
    ports:
      - "3000:3000"
    volumes:
      - broadside-data:/data
    environment:
      - RUST_LOG=broadside=info

volumes:
  broadside-data:
```

### Requirements

- A domain with DNS pointing to your server
- A reverse proxy (Caddy or nginx) for TLS termination
- Nothing else. No PostgreSQL, no Redis, no Node.js.

Broadside uses SQLite for storage. PostgreSQL support is planned for environments that require managed databases (e.g., RDS/Aurora).

### CDN / Caching

Broadside sets per-endpoint `Cache-Control` headers so a CDN or caching reverse proxy works out of the box:

- **Actor, WebFinger, followers, NodeInfo**: `public, max-age=300` (5 min)
- **Outbox pages, index, profile**: `public, max-age=60` (1 min)
- **Inbox POST, webhook, health**: `no-store`

Content-negotiated endpoints (actor serves JSON-LD or HTML based on `Accept`) include `Vary: Accept` so CDNs cache both variants correctly. No purge API needed — the short TTLs mean new posts and follows appear within a minute.

### Caddy example

```
corp.example {
    reverse_proxy 127.0.0.1:3000
}
```

### Backup

Copy the SQLite file and `media/` directory. That's the entire instance state.

```bash
sqlite3 /var/lib/broadside/broadside.db ".backup /backup/broadside.db"
cp -r /var/lib/broadside/media/ /backup/media/
```

### Monitoring

```bash
broadside status                  # CLI: personas, followers, queue depth
curl http://localhost:3000/health  # JSON health endpoint
```

## CLI reference

```
broadside init <data_dir>                          # Create data dir, DB, sample config
broadside persona add <username>                   # Generate keypair, create actor
broadside persona list                             # List personas with follower counts
broadside persona update <username> [options]      # Update display name, bio, avatar, metadata
broadside post --persona=<name> <content>          # Publish a post
broadside post --persona=<name> --markdown         # Read markdown from stdin
broadside post --persona=<name> --media=<path> ... # Attach images
broadside queue inspect                            # Show pending/dead deliveries
broadside queue retry                              # Retry all dead-lettered deliveries
broadside queue stats                              # Delivery statistics
broadside followers list --persona=<name>          # List followers
broadside followers count                          # Follower counts per persona
broadside feed-poll                                # One-shot poll of all configured feeds
broadside status                                   # Overall health
broadside serve                                    # Start HTTP server
```

## Security

Broadside's attack surface is deliberately minimal:

- **Inbox**: requires HTTP Signature, verifies Digest and Date headers, rejects unsigned/stale/tampered requests
- **Webhook**: key authenticated via `Authorization: Bearer` header (constant-time comparison)
- **SSRF protection**: all outbound fetches require HTTPS, block private/link-local/metadata IPs, no HTTP redirect following
- **Media**: MIME sniffing (not extension trust), 10 MB limit, 64 MB decoded pixel cap (decompression bomb protection), EXIF stripping
- **Content**: HTML sanitized via ammonia (Mastodon-compatible allowlist)
- **Rate limiting**: per-IP token bucket on inbox endpoints
- **Graceful shutdown**: SIGTERM/SIGINT handled cleanly

There is no OAuth, no client API, no admin UI, no login form, no session management.

## Building from source

```bash
cargo build --release
# Binary at target/release/broadside (14 MB)
```

### Quality gates

```bash
cargo fmt --all
cargo clippy -- -D warnings
cargo test
cargo audit
```

### Fuzz testing

```bash
cargo +nightly fuzz run fuzz_sanitize
cargo +nightly fuzz run fuzz_signature_parser
cargo +nightly fuzz run fuzz_image_sniff
```

## License

AGPL-3.0
