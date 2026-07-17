# Broadside

One-way ActivityPub server for organizations. Publish to the fediverse without running Mastodon.

Broadside gives your organization a federated presence (`@announcements@corp.example`) that people can follow from any Mastodon, Misskey, or GoToSocial account. You publish via CLI, RSS feed, webhook, or file drop. There is no client API, no user login, no timeline to read.

## What it does

- Federates posts to followers via ActivityPub
- Accepts follows automatically
- Serves actor profiles and WebFinger
- Supports multiple personas per domain (`@engineering@`, `@blog@`, `@releases@`)

## What it doesn't do

- No Mastodon Client API. No OAuth. No web UI.
- No inbound content. Replies and likes from followers are accepted and discarded.
- No timelines, notifications, or streaming.
- No moderation tools. There is no community to moderate.

## Posting

```bash
# CLI
broadside post --persona=announcements "We shipped v2.0 today."
broadside post --persona=engineering --markdown < release-notes.md

# Or configure an RSS feed, webhook, or watched directory in config.toml
```

## Install

Single binary. SQLite for storage. TLS via reverse proxy.

```bash
broadside init /var/lib/broadside
broadside persona add announcements --display-name="ACME Announcements"
broadside serve
```

## Requirements

- A domain with DNS pointing to your server
- A reverse proxy (Caddy or nginx) for TLS
- Nothing else

## Shared code

Broadside shares the `fieldwork` crate with [smallhold](../smallhold/), a full Mastodon-compatible personal server. fieldwork provides HTTP signatures, ActivityPub delivery, WebFinger, and actor document handling.

## License

AGPL-3.0
