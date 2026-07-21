# Broadside

One-way ActivityPub server for organizations. Publish to the fediverse without running Mastodon.

Broadside gives your organization a federated presence (`@announcements@corp.example`) that people can follow from any ActivityPub client. You publish via CLI, RSS feed, webhook, or file drop. Broadside federates the content. There is no login, no timeline to read.

Followers see your content in whatever fediverse app they use.

## Client compatibility (read-only)

Broadside serves content through every major fediverse client API:

| API | What followers see | Endpoints |
|-----|-------------------|-----------|
| **Mastodon** | Posts in timeline | Actor, outbox, WebFinger, NodeInfo |
| **Pixelfed** | Photo galleries | Albums, discover, trending |
| **Lemmy** | Community posts | Communities, posts, comments |
| **PeerTube** | Video channels | Videos, channels |
| **Misskey** | Notes | Local timeline |
| **Funkwhale** | Audio tracks | Tracks, albums, channels, playlists |
| **Bookwyrm** | Book reviews | Books, reviews, reading activity |
| **WriteFreely** | Blog articles | Posts, collections, markdown |

All read-only — no write endpoints, no OAuth, no auth required for public content.

## Quick start

```bash
broadside init /var/lib/broadside
broadside persona add announcements --display-name="ACME Announcements"
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
```

Or configure automatic ingestion:

| Method | How | When to use |
|--------|-----|-------------|
| **CLI** | `broadside post --persona=NAME "content"` | Manual posts, scripting, cron |
| **RSS/Atom** | Configure feed URL in `config.toml` | Auto-federate blog posts |
| **Webhook** | `POST /hook/{persona}` with Bearer token | CI/CD, CMS, internal tools |
| **Directory watch** | Drop a `.md` file in a watched directory | Simple file-based workflow |

## Web pages

- Profile pages with posts, stats, metadata fields
- Photo gallery (`/users/{name}/photos`) — responsive CSS grid with lazy loading
- DID document (`/users/{name}/did.json`)
- Content negotiation — JSON-LD for AP clients, HTML for browsers
- Dark mode via `prefers-color-scheme`
- Brand theming via W3C Design Tokens or custom CSS

## What it does

- Federates posts to followers via ActivityPub
- Accepts follows automatically
- Read-only compatibility with every major fediverse client API
- DID support (did:scid, did:key, did:web) with BIP-39 mnemonic recovery
- Media attachments with MIME validation, EXIF stripping, resize, and blurhash
- Delivery retry with exponential backoff and per-domain circuit breaker
- CDN-friendly caching headers

## What it doesn't do

- No write client API. No OAuth. No web UI for posting.
- No inbound content processing. Replies, likes, and boosts are accepted and discarded.
- No timelines, notifications, or streaming.

## Configuration

```toml
[server]
bind = "127.0.0.1:3000"
domain = "corp.example"
data_dir = "/var/lib/broadside"

[[feed]]
persona = "blog"
url = "https://corp.example/blog/feed.xml"
poll_interval = "15m"

[[webhook]]
persona = "releases"
key = "change-this-to-a-real-secret"
```

## Architecture

Built on [fieldwork](https://github.com/MarkAtwood/fedistract) — shared fediverse building blocks used by all three servers (broadside, smallhold, gaja).

```
reverse proxy (Caddy / nginx)
         |
broadside binary  (axum, tokio)
  ├── Read-only Client APIs (Mastodon, Pixelfed, Lemmy, PeerTube, Misskey, Funkwhale, Bookwyrm, WriteFreely)
  ├── ActivityPub S2S (inbox, outbox, actors, DID)
  ├── WebFinger / NodeInfo
  ├── Web pages (profiles, photo grid)
  ├── Content ingestion (RSS, webhook, file watch)
  ├── SQLite via sqlx (WAL mode)
  └── Delivery worker (retry, circuit breaker)
```

## Deployment

Broadside **must** run behind a reverse proxy for TLS.

```
corp.example {
    reverse_proxy localhost:3000
}
```

### Requirements

- A domain with DNS pointing to your server
- A reverse proxy (Caddy or nginx) for TLS
- Nothing else. No PostgreSQL, no Redis, no Node.js.

### Backup

Copy the SQLite file and `media/` directory:

```bash
sqlite3 /var/lib/broadside/broadside.db ".backup /backup/broadside.db"
cp -r /var/lib/broadside/media/ /backup/media/
```

## Security

- SSRF protection on all outbound HTTP
- HTTP Signature verification with Digest and Date freshness checks
- HTML sanitization via ammonia
- Media: MIME sniffing, EXIF stripping, decompression bomb limits
- Rate limiting on inbox endpoints
- Security headers (CSP, X-Frame-Options, Referrer-Policy)
- No OAuth, no login form, no session management — minimal attack surface

## License

AGPL-3.0.
