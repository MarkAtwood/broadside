# Broadside: Design Document

## Overview

Broadside is a one-way ActivityPub server for organizations. It publishes content to the fediverse. People follow it. It does not follow back, read replies, or expose a client API.

This document covers architecture, data model, protocol behavior, and implementation decisions.

## Architecture

```
                          ┌──────────────────────────────────┐
                          │        reverse proxy (caddy)     │
                          │           TLS termination        │
                          └───────────────┬──────────────────┘
                                          │
┌─────────────────────────────────────────▼──────────────────────────────────┐
│                          broadside binary (axum)                           │
│                                                                            │
│  Inbound HTTP                          Ingestion                           │
│  ┌──────────────────┐                  ┌───────────────────────────────┐   │
│  │ GET  /.well-known/│                  │ CLI (post subcommand)        │   │
│  │      webfinger    │                  │ RSS/Atom poller (tokio task) │   │
│  │ GET  /users/{id}  │                  │ Webhook (POST /hook/{persona})│  │
│  │ POST /inbox       │                  │ Directory watcher (notify)   │   │
│  │ POST /users/{id}/ │                  └──────────────┬──────────────┘   │
│  │      inbox        │                                  │                  │
│  │ GET  /health      │                                  │                  │
│  └────────┬─────────┘                                  │                  │
│           │                                             │                  │
│  ┌────────▼─────────────────────────────────────────────▼──────────────┐   │
│  │                        core domain                                  │   │
│  │  create_post() ──▶ insert posts row ──▶ fan out to delivery_queue  │   │
│  │  handle_follow() ──▶ verify sig ──▶ insert follower ──▶ send Accept│   │
│  │  handle_undo_follow() ──▶ verify sig ──▶ delete follower row       │   │
│  └────────┬───────────────────────────────────────────────────────────┘   │
│           │                                                                │
│  ┌────────▼───────────────────────────────────────────────────────────┐   │
│  │                     delivery worker (tokio task)                    │   │
│  │  polls delivery_queue, HTTP POST with signatures, retry + backoff  │   │
│  └────────┬───────────────────────────────────────────────────────────┘   │
│           │                                                                │
│  ┌────────▼──────────┐  ┌──────────────┐                                  │
│  │  SQLite (WAL)     │  │  media/      │                                  │
│  │  sqlx + FK        │  │  filesystem  │                                  │
│  └───────────────────┘  └──────────────┘                                  │
└───────────────────────────────────────────────────────────────────────────┘
```

## Source layout

All code lives in a single `broadside` crate. Shared abstractions will be extracted into a `fieldwork` crate when a second consumer (smallhold) needs them.

| Module | Responsibility |
|---|---|
| `signatures` | HTTP signature generation (POST) and verification (inbox) |
| `delivery` | Delivery queue worker: dequeue, POST, retry, backoff, circuit breaker, dead-letter |
| `actor_cache` | Remote actor public key fetch and cache (24h TTL, owner validation) |
| `persona` | Persona CRUD, RSA 2048 keypair generation (OsRng) |
| `post` | Post creation, text-to-HTML conversion |
| `content` | Hashtag/mention/URL detection, AP `tag` array generation |
| `media` | Image processing: MIME sniff, resize, EXIF strip, blurhash, decompression bomb limits |
| `sanitize` | HTML sanitization (ammonia, Mastodon-compatible allowlist), markdown rendering |
| `config` | `config.toml` parsing and validation |
| `db` | SQLite connection pool (WAL, foreign keys), schema initialization |
| `server` | Axum HTTP server: all routes, content negotiation, SSRF guards, rate limiting |
| `webhook` | Webhook ingestion endpoint |
| `feed` | RSS/Atom feed poller |
| `watch` | Directory watcher (notify crate) |
| `ratelimit` | Per-IP token bucket rate limiter |
| `id` | Snowflake-style ID generation |

## Data model

Six tables.

### personas

```sql
CREATE TABLE personas (
    id           TEXT PRIMARY KEY,
    username     TEXT NOT NULL UNIQUE,
    display_name TEXT NOT NULL DEFAULT '',
    bio          TEXT NOT NULL DEFAULT '',
    avatar_path  TEXT,
    header_path  TEXT,
    metadata     TEXT NOT NULL DEFAULT '[]',  -- JSON array of {name, value}
    private_key  TEXT NOT NULL,               -- PEM, RSA 2048
    public_key   TEXT NOT NULL,               -- PEM
    created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
```

### followers

```sql
CREATE TABLE followers (
    id               TEXT PRIMARY KEY,
    persona_id       TEXT NOT NULL REFERENCES personas(id),
    actor_uri        TEXT NOT NULL,
    inbox_uri        TEXT NOT NULL,
    shared_inbox_uri TEXT,
    followed_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    UNIQUE(persona_id, actor_uri)
);
```

### posts

```sql
CREATE TABLE posts (
    id            TEXT PRIMARY KEY,
    persona_id    TEXT NOT NULL REFERENCES personas(id),
    content_html  TEXT NOT NULL,
    content_text  TEXT NOT NULL,
    published_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    source_ref    TEXT,
    UNIQUE(persona_id, source_ref)
);
```

### post_media

```sql
CREATE TABLE post_media (
    id          TEXT PRIMARY KEY,
    post_id     TEXT NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
    file_path   TEXT NOT NULL,
    mime_type   TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    blurhash    TEXT NOT NULL DEFAULT '',
    width       INTEGER,
    height      INTEGER
);
```

### delivery_queue

```sql
CREATE TABLE delivery_queue (
    id          TEXT PRIMARY KEY,
    post_id     TEXT NOT NULL REFERENCES posts(id),
    inbox_uri   TEXT NOT NULL,
    attempts    INTEGER NOT NULL DEFAULT 0,
    next_retry  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    status      TEXT NOT NULL DEFAULT 'pending',  -- pending, dead
    last_error  TEXT
);
CREATE INDEX idx_delivery_pending ON delivery_queue(status, next_retry);
```

### feed_state

```sql
CREATE TABLE feed_state (
    feed_url     TEXT PRIMARY KEY,
    persona_id   TEXT NOT NULL REFERENCES personas(id),
    last_seen_id TEXT,
    last_poll    TEXT
);
```

## ActivityPub behavior

### Outbound activities

Broadside produces exactly two activity types:

**Create{Note}** — when a post is created via any ingestion surface:
```json
{
  "@context": "https://www.w3.org/ns/activitystreams",
  "id": "https://corp.example/users/announcements/statuses/123/activity",
  "type": "Create",
  "actor": "https://corp.example/users/announcements",
  "published": "2026-06-13T12:00:00Z",
  "to": ["https://www.w3.org/ns/activitystreams#Public"],
  "cc": ["https://corp.example/users/announcements/followers"],
  "object": {
    "id": "https://corp.example/users/announcements/statuses/123",
    "type": "Note",
    "attributedTo": "https://corp.example/users/announcements",
    "content": "<p>We shipped v2.0 today. <a href=\"https://corp.example/tags/release\" class=\"mention hashtag\" rel=\"tag\">#release</a></p>",
    "published": "2026-06-13T12:00:00Z",
    "to": ["https://www.w3.org/ns/activitystreams#Public"],
    "cc": ["https://corp.example/users/announcements/followers"],
    "tag": [
      {"type": "Hashtag", "href": "https://corp.example/tags/release", "name": "#release"}
    ],
    "attachment": []
  }
}
```

**Accept{Follow}** — automatic response to every inbound Follow (sent asynchronously in a background task):
```json
{
  "@context": "https://www.w3.org/ns/activitystreams",
  "id": "https://corp.example/users/announcements#accept/456",
  "type": "Accept",
  "actor": "https://corp.example/users/announcements",
  "object": { "type": "Follow", "id": "...", "actor": "...", "object": "..." }
}
```

No other outbound activities. No Update, no Delete, no Announce, no Like.

### Inbound activities

| Activity | Behavior |
|---|---|
| `Follow` | Require valid HTTP Signature + Digest + Date. Verify actor-keyId match. Insert follower. Send `Accept` asynchronously. |
| `Undo{Follow}` | Require valid HTTP Signature. Delete follower row (keyed on signed actor, not body actor). |
| Everything else | Require valid HTTP Signature. Return 202 Accepted. Do nothing. |
| No Signature header | Return 401 Unauthorized. |

### Actor document

Served at `GET /users/{username}` with content negotiation:
- `Accept: application/activity+json` → JSON-LD actor document
- `Accept: text/html` → HTML profile page (for "view on original site" links)

The actor document includes: `published` (join date), `discoverable`, `manuallyApprovesFollowers` (false), `endpoints.sharedInbox`, `publicKey`, and optional `icon`, `image`, and `attachment` (PropertyValue metadata fields).

### Endpoints

| Path | Method | Purpose |
|---|---|---|
| `/` | GET | Index page listing all personas (HTML) |
| `/.well-known/webfinger` | GET | Actor discovery |
| `/.well-known/nodeinfo` | GET | NodeInfo discovery |
| `/nodeinfo/2.0` | GET | NodeInfo document |
| `/users/{username}` | GET | Actor document (JSON-LD) or profile page (HTML) |
| `/users/{username}/inbox` | POST | Per-actor inbox (signed) |
| `/users/{username}/outbox` | GET | Paginated outbox (public posts) |
| `/users/{username}/followers` | GET | Followers collection (count only, no enumeration) |
| `/inbox` | POST | Shared inbox (signed) |
| `/hook/{persona}` | POST | Webhook ingestion (Bearer token authenticated) |
| `/health` | GET | Health check (JSON) |

## Ingestion surfaces

### CLI

Always available. The binary is both server and client — CLI commands write directly to SQLite (server does not need to be running for `broadside post`).

```
broadside post --persona=NAME CONTENT
broadside post --persona=NAME --markdown < file.md
broadside post --persona=NAME --media=path1.png --media=path2.jpg CONTENT
```

The CLI inserts the post row, processes media (validate, resize, blurhash), and fans out to the delivery queue. If the server is running, the delivery worker picks it up immediately. If not, deliveries are processed on next `broadside serve` start.

### RSS/Atom poller

Configured in `config.toml`. A tokio task polls each feed on its interval.

```toml
[[feed]]
persona = "blog"
url = "https://corp.example/blog/feed.xml"
poll_interval = "15m"
```

Entry-to-Note mapping:
- `<title>` → first line of content (bold)
- `<description>` or `<content:encoded>` → body (sanitized HTML via ammonia)
- `<enclosure>` or `<media:content>` with image MIME → media attachment (capped at 8 per entry)
- `<link>` → appended as footer link (scheme-validated, sanitized)
- `<id>` or `<guid>` → `source_ref` for dedup (INSERT OR IGNORE)

Content length cap: 5000 characters after sanitization. Entries exceeding this are truncated at a UTF-8 safe boundary, re-sanitized to close open tags, and appended with a "read more" link.

### Webhook

Optional, enabled per-persona in config:

```toml
[[webhook]]
persona = "releases"
key = "random-secret-here"
```

Endpoint: `POST /hook/{persona}` with `Authorization: Bearer {key}` or `X-Webhook-Key: {key}` header.

```json
{
  "content": "We shipped v2.0.",
  "content_type": "text/plain",
  "media": [
    {"url": "https://cdn.corp.example/img.png", "description": "screenshot"}
  ]
}
```

`content_type` is optional, defaults to `text/plain`. If `text/markdown`, content is rendered to HTML via pulldown-cmark before sanitization.

Media URLs are fetched (with SSRF guard), validated, cached locally. Timeout: 30 seconds. Max size: 10 MB. Max 8 media items per post. Image types only.

### Directory watcher

Optional, enabled in config:

```toml
[watch]
persona = "announcements"
path = "/var/spool/broadside/incoming/"
published = "/var/spool/broadside/published/"
pattern = "*.md"
```

Uses `notify` crate for filesystem events. Symlinks are rejected. New files matching the pattern are:
1. Read as markdown
2. Rendered to HTML and sanitized
3. Federated as a Note
4. Moved to `published/` directory (even on dedup/error, to prevent stranding)

## Content processing

All content (from any ingestion surface) passes through `content::process_content()` at delivery/outbox serialization time:

1. **Bare URLs** → auto-linked as `<a>` tags
2. **Hashtags** (`#word`) → linked as `<a class="mention hashtag" rel="tag">` with `tag` array entry (`type: Hashtag`)
3. **Mentions** (`@user@domain`) → linked as `<span class="h-card"><a class="u-url mention">` with `tag` array entry (`type: Mention`)

This ensures hashtags are clickable in Mastodon, mentions trigger notifications, and URLs are rendered correctly.

## Delivery

### Fan-out

When a post is created, broadside queries the followers table for the posting persona, deduplicates by `shared_inbox_uri` (falling back to `inbox_uri`), and inserts one `delivery_queue` row per unique inbox.

### Retry schedule

| Attempt | Delay |
|---|---|
| 1 | immediate |
| 2 | 1 minute |
| 3 | 5 minutes |
| 4 | 30 minutes |
| 5 | 2 hours |
| 6 | 8 hours |
| 7 | dead-letter |

A `410 Gone` response marks the delivery dead immediately and removes the follower (scoped to the posting persona only).

### Circuit breaker

Per-domain. Ten consecutive failures to any inbox on a domain triggers a one-hour pause for all deliveries to that domain. After the cooldown, the failure counter resets so the domain gets a fair retry window. The breaker state is in-memory (resets on restart, which is fine — the retry schedule handles persistence).

### Delivery-time validation

Inbox URIs are re-validated at delivery time (HTTPS required, private IPs blocked) to defend against DNS rebinding after the initial Follow-accept.

## Security

### Inbox authentication

All inbound inbox requests MUST include:
- `Signature` header — verified against the actor's public key (fetched and cached with 24h TTL, owner validated)
- `Digest` header — SHA-256 body hash verified (prevents body substitution)
- `Date` header — must be within 5 minutes of server time (prevents replay)
- Actor-keyId match — the signing key's actor URI must match the activity's `actor` field

Unsigned requests are rejected with 401. Failed verification is fail-closed (retry once after cache invalidation for key rotation).

### SSRF protection

Every outbound HTTP fetch (actor documents, media, Accept delivery) is guarded:
- HTTPS required (no plaintext HTTP)
- No automatic redirect following (reqwest `Policy::none()`)
- Private/link-local/loopback IPs blocked (IPv4 + IPv6 + IPv4-mapped)
- AWS metadata endpoint (169.254.x.x) blocked
- Common private hostnames blocked (localhost, .local, .internal)
- Actor document responses capped at 64 KB

### Content sanitization

All HTML content (from RSS feeds, webhook markdown, and user input) is sanitized with ammonia before storage. The allowlist matches Mastodon's: `<p>`, `<br>`, `<a>` (href only), `<span>`, `<em>`, `<strong>`, `<del>`, `<blockquote>`, `<code>`, `<pre>`, `<ul>`, `<ol>`, `<li>`. Everything else is stripped.

### Media validation

Images are validated by magic-byte MIME sniffing (not extension trust), capped at 10 MB download and 64 MB decoded pixels (decompression bomb protection via `image::Limits`), stripped of EXIF metadata by re-encoding, and stored with computed blurhash.

### Webhook authentication

Webhook keys are transmitted via `Authorization: Bearer` header (not query string, to prevent leaking to access logs). Comparison uses SHA-256 hash then constant-time `ct_eq` to prevent both timing oracle and key-length leakage.

### Rate limiting

Per-IP token bucket on inbox endpoints (60 requests/minute). Keys on `X-Real-IP` header (must be set by reverse proxy). Stale buckets pruned every 10 minutes.

### Operational

The server warns at startup if `broadside.db` or `config.toml` are world-readable. Signing error messages are sanitized before storage in the delivery queue to prevent key material leakage.

## Configuration

Single file: `config.toml`.

```toml
[server]
bind = "127.0.0.1:3000"
domain = "corp.example"
data_dir = "/var/lib/broadside"

# Optional ingestion surfaces
[[feed]]
persona = "blog"
url = "https://corp.example/blog/feed.xml"
poll_interval = "15m"

[[webhook]]
persona = "releases"
key = "change-this-to-a-real-secret"

[watch]
persona = "announcements"
path = "/var/spool/broadside/incoming/"
published = "/var/spool/broadside/published/"
pattern = "*.md"
```

## CLI commands

```
broadside init <data_dir>                           # create data dir, empty DB, sample config
broadside persona add <username>                    # generate RSA 2048 keypair, create actor
broadside persona list                              # list personas with follower counts
broadside persona update <username> [options]       # update display name, bio, avatar, metadata
broadside post --persona=<name> <content>           # publish a post
broadside post --persona=<name> --markdown          # read markdown from stdin
broadside post --persona=<name> --media=<path> ...  # attach images
broadside queue inspect                             # show pending/dead deliveries
broadside queue retry                               # retry all dead-lettered deliveries
broadside queue stats                               # delivery success/failure counts
broadside followers list --persona=<name>           # list followers
broadside followers count                           # follower counts per persona
broadside feed-poll                                 # one-shot poll of all configured feeds
broadside status                                    # overall health: personas, followers, queue
broadside serve                                     # start HTTP server
```

## Known limitations

- **RSA 2048 key size**: the `rsa` crate has a known timing side-channel advisory (RUSTSEC-2023-0071, Marvin Attack). Broadside only uses signing (not decryption), limiting exposure. No upstream fix is available. Monitor for a fix or consider migration to ed25519 when the ActivityPub ecosystem supports it.
- **DNS rebinding**: SSRF guards check the hostname string before DNS resolution. A hostname that resolves to a private IP after the initial check could bypass the guard. Full mitigation requires resolver-level DNS rebinding protection.
- **No Delete activity**: posts cannot be retracted once federated. A future version may add Delete support.
- **Private keys stored plaintext**: RSA private keys are stored as PEM text in SQLite. File permissions on the database file are the primary protection. At-rest encryption would require passphrase or HSM integration.
