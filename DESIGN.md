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
│  │  handle_follow() ──▶ insert followers row ──▶ send Accept          │   │
│  │  handle_undo_follow() ──▶ delete followers row                     │   │
│  └────────┬───────────────────────────────────────────────────────────┘   │
│           │                                                                │
│  ┌────────▼───────────────────────────────────────────────────────────┐   │
│  │                     delivery worker (tokio task)                    │   │
│  │  polls delivery_queue, HTTP POST with signatures, retry + backoff  │   │
│  └────────┬───────────────────────────────────────────────────────────┘   │
│           │                                                                │
│  ┌────────▼──────────┐  ┌──────────────┐                                  │
│  │  SQLite (WAL)     │  │  media/      │                                  │
│  │  sqlx             │  │  filesystem  │                                  │
│  └───────────────────┘  └──────────────┘                                  │
└───────────────────────────────────────────────────────────────────────────┘
```

## Shared crate: fieldwork

The `fieldwork` crate is shared with smallhold. It provides:

| Module | Responsibility |
|---|---|
| `fieldwork::signatures` | HTTP signature generation (POST) and verification (inbox) |
| `fieldwork::delivery` | Delivery queue worker: dequeue, POST, retry, backoff, circuit breaker, dead-letter |
| `fieldwork::actor` | Actor document serialization (JSON-LD), keypair generation (RSA 2048) |
| `fieldwork::webfinger` | WebFinger response builder and `acct:` URI parsing |
| `fieldwork::fetch` | Remote actor fetch, JSON-LD parsing, caching |
| `fieldwork::db` | SQLite connection pool, migration runner, common query helpers |
| `fieldwork::nodeinfo` | NodeInfo 2.0 and 2.1 response builder |

fieldwork does NOT contain:
- Mastodon Client API types (smallhold only)
- OAuth (smallhold only)
- Timeline computation (smallhold only)
- Ingestion surfaces (broadside only)
- Any application-level business logic

## Data model

Six tables. No more.

### personas

```sql
CREATE TABLE personas (
    id          TEXT PRIMARY KEY,  -- snowflake
    username    TEXT NOT NULL UNIQUE,
    display_name TEXT NOT NULL DEFAULT '',
    bio         TEXT NOT NULL DEFAULT '',
    avatar_path TEXT,
    header_path TEXT,
    private_key TEXT NOT NULL,     -- PEM, RSA 2048
    public_key  TEXT NOT NULL,     -- PEM
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
```

### followers

```sql
CREATE TABLE followers (
    id              TEXT PRIMARY KEY,
    persona_id      TEXT NOT NULL REFERENCES personas(id),
    actor_uri       TEXT NOT NULL,
    inbox_uri       TEXT NOT NULL,
    shared_inbox_uri TEXT,
    followed_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    UNIQUE(persona_id, actor_uri)
);
```

### posts

```sql
CREATE TABLE posts (
    id            TEXT PRIMARY KEY,  -- snowflake
    persona_id    TEXT NOT NULL REFERENCES personas(id),
    content_html  TEXT NOT NULL,
    content_text  TEXT NOT NULL,     -- plain text for Atom, previews
    published_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    source_ref    TEXT,             -- dedup key: feed guid, webhook idempotency key, file path
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
    "content": "<p>We shipped v2.0 today.</p>",
    "published": "2026-06-13T12:00:00Z",
    "to": ["https://www.w3.org/ns/activitystreams#Public"],
    "cc": ["https://corp.example/users/announcements/followers"],
    "attachment": []
  }
}
```

**Accept{Follow}** — automatic response to every inbound Follow:
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
| `Follow` | Validate signature. Insert follower. Send `Accept`. |
| `Undo{Follow}` | Validate signature. Delete follower row. |
| Everything else | Validate signature. Return 202 Accepted. Do nothing. |

Signature validation prevents spoofed unfollows. All other inbound activities are accepted to prevent remote servers from retrying, but are not processed or stored.

### Endpoints

| Path | Method | Purpose |
|---|---|---|
| `/.well-known/webfinger` | GET | Actor discovery |
| `/.well-known/nodeinfo` | GET | NodeInfo discovery |
| `/nodeinfo/2.0` | GET | NodeInfo document |
| `/users/{username}` | GET | Actor document (JSON-LD) |
| `/users/{username}/inbox` | POST | Per-actor inbox |
| `/users/{username}/outbox` | GET | Paginated outbox (public posts) |
| `/users/{username}/followers` | GET | Followers collection (count only, no enumeration) |
| `/inbox` | POST | Shared inbox |
| `/hook/{persona}` | POST | Webhook ingestion (key-authenticated) |
| `/health` | GET | Health check (JSON) |

The outbox endpoint is required for federation correctness — some servers fetch it to backfill content when a follow is established.

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
- `<enclosure>` or `<media:content>` with image MIME → media attachment
- `<link>` → appended as footer link
- `<id>` or `<guid>` → `source_ref` for dedup

Content length cap: 5000 characters after sanitization. Entries exceeding this are truncated with a "read more" link to the original.

### Webhook

Optional, enabled per-persona in config:

```toml
[[webhook]]
persona = "releases"
key = "random-secret-here"
```

Endpoint: `POST /hook/{persona}?key={secret}`

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

Media URLs are fetched, validated, cached locally. Timeout: 30 seconds. Max size: 10 MB. Image types only.

### Directory watcher

Optional, enabled in config:

```toml
[watch]
persona = "announcements"
path = "/var/spool/broadside/incoming/"
published = "/var/spool/broadside/published/"
pattern = "*.md"
```

Uses `notify` crate for filesystem events. New files matching the pattern are:
1. Read as markdown
2. Rendered to HTML
3. Federated as a Note
4. Moved to `published/` directory

If images are referenced as relative paths in the markdown, they are resolved relative to the incoming directory, validated, and attached.

## Delivery

Delivery is handled by the `fieldwork::delivery` module, shared with smallhold.

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

A `410 Gone` response marks the delivery dead immediately and removes the follower.

### Circuit breaker

Per-domain. Ten consecutive failures to any inbox on a domain triggers a one-hour pause for all deliveries to that domain. The breaker state is in-memory (resets on restart, which is fine — the retry schedule handles persistence).

## Security

### Attack surface

Broadside's attack surface is deliberately minimal:

- **Inbox** — accepts signed HTTP POSTs. Signature verification rejects spoofed activities. Valid activities are accepted but only Follow and Undo Follow are processed; everything else is discarded. There is no stored-XSS risk because inbound content is never stored.
- **Webhook** — pre-shared key authentication. Should be behind a firewall or reverse proxy with IP allowlisting.
- **WebFinger / actor / outbox** — read-only GET endpoints serving deterministic content. No user input in responses.
- **Media** — images are validated on ingest (MIME sniffing, dimension limits, EXIF stripping). No user-uploaded media via HTTP (CLI and webhook only).

There is no OAuth, no client API, no admin UI, no login form, no session management.

### Content sanitization

Outbound content from RSS feeds and webhook markdown is sanitized with ammonia before storage. The sanitization policy matches Mastodon's: allow `<p>`, `<br>`, `<a>`, `<span>`, basic formatting tags. Strip everything else.

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
broadside init <data_dir>                    # create data dir, empty DB, sample config
broadside persona add <username>             # generate keypair, create actor
broadside persona list                       # list personas with follower counts
broadside persona update <username>          # update display name, bio, avatar
broadside post --persona=<name> <content>    # publish a post
broadside post --persona=<name> --markdown   # read markdown from stdin
broadside queue inspect                      # show pending/dead deliveries
broadside queue retry                        # retry all dead-lettered deliveries
broadside queue stats                        # delivery success/failure counts
broadside followers list --persona=<name>    # list followers
broadside followers count                    # follower counts per persona
broadside feed poll                          # one-shot poll of all configured feeds
broadside status                             # overall health: personas, followers, queue
broadside serve                              # start HTTP server
```

## Build phases

1. **fieldwork crate** — extract shared code from smallhold (or build fresh if smallhold hasn't started). HTTP signatures, delivery worker, actor, WebFinger, SQLite utilities.
2. **Core** — data model, persona management, post creation, delivery fan-out. CLI `init`, `persona`, `post`, `queue`, `serve`.
3. **Federation** — inbox handler (Follow, Undo Follow, accept-and-discard), outbox endpoint, WebFinger, NodeInfo.
4. **Ingestion** — RSS/Atom poller, webhook endpoint, directory watcher. Each is independent and can be built in parallel.
5. **Hardening** — rate limiting on inbox, signature verification edge cases, media validation, integration tests against mastodon.social.
