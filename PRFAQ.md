# PRFAQ: Broadside

---

## PRESS RELEASE

**FOR IMMEDIATE RELEASE**

**Broadside Gives Organizations a Fediverse Presence in Ten Minutes, No Mastodon Required**

*Seattle, WA* — **Broadside** is an open-source server that lets any organization publish to the fediverse as a single binary with zero infrastructure dependencies. People on Mastodon, Misskey, GoToSocial, and any other ActivityPub service can follow an organization's broadside account and see its posts in their home timeline — no special client, no account on a corporate platform, no algorithm.

Organizations post content through whichever surface fits their workflow: a CLI command, an RSS feed that broadside polls automatically, a webhook from a CI pipeline, or a markdown file dropped in a directory. Broadside federates the content to followers using standard ActivityPub. From a follower's perspective, the account looks and behaves like any other fediverse account — with a profile page, metadata fields, avatar, and clickable hashtags.

"Organizations keep asking how to 'get on Mastodon' and the answer is always 'run a full Mastodon stack or beg for an account on someone else's instance,'" said the project author. "Broadside is the third option: own your identity, publish your content, skip everything else."

A working deployment requires one binary, one config file, one SQLite database, and a reverse proxy for TLS. There is no PostgreSQL, no Redis, no Sidekiq, no Elasticsearch, no Node.js asset pipeline. Memory footprint is under 50 MB. Cold start is under one second.

Broadside supports multiple personas under one domain. A company can run `@announcements@corp.example`, `@engineering@corp.example`, and `@careers@corp.example` as independent ActivityPub actors — each with its own followers, posting cadence, and content source — from a single process on a single machine.

Broadside has been tested live against mastodon.social with full federation working: follower discovery, follow/accept, signed post delivery, hashtags, mentions, media attachments, and profile metadata.

Broadside is released under the AGPL-3.0 license.

---

## FREQUENTLY ASKED QUESTIONS

### Customer / User FAQs

**Q: Who is this for?**

A: Any organization that wants a fediverse presence for broadcasting — press releases, blog posts, product updates, engineering announcements, status page notifications — without operating a full social media server. The organization publishes; followers consume. There is no two-way conversation, no DMs, no moderation queue.

**Q: Can people reply to our posts?**

A: People can reply from their own clients, and their replies are visible to their own followers. Broadside accepts the inbound activity (so remote servers don't retry) but does not store, display, or forward replies. If you need two-way engagement, run Mastodon or GoToSocial.

**Q: Can we use our existing domain?**

A: Yes. Broadside serves WebFinger and actor documents on your domain. Your accounts are `@name@yourdomain.example`. You own the identity.

**Q: What if we already have a website on that domain?**

A: Your reverse proxy routes `/.well-known/webfinger`, `/.well-known/nodeinfo`, `/users/*`, and `/inbox` to broadside. Everything else goes to your existing web server. Broadside also serves a simple HTML profile page when browsers visit `/users/{name}`, so the "view on original site" link in Mastodon works.

**Q: Can people on mastodon.social follow us?**

A: Yes. That is the primary compatibility target, tested and verified. A user on any ActivityPub server searches for `@name@yourdomain.example`, clicks follow, and sees your posts in their home timeline. Hashtags are clickable, mentions generate notifications, and links are rendered correctly.

**Q: How do we post?**

A: Four options, mix and match:
1. **CLI**: `broadside post --persona=blog "text"` or pipe markdown from stdin, attach images with `--media`
2. **RSS/Atom feed**: point broadside at your blog's feed, it federates new entries automatically with media attachments
3. **Webhook**: POST JSON to a keyed endpoint from your CI, CMS, or internal tools
4. **Directory watch**: drop a markdown file in a directory, broadside federates and archives it

**Q: Do hashtags and mentions work?**

A: Yes. `#hashtag` in your post content becomes a clickable hashtag in Mastodon. `@user@instance` becomes a clickable mention that triggers a notification. URLs are auto-linked. All of this is handled automatically — no special markup needed.

**Q: Can we add profile metadata like Website and GitHub links?**

A: Yes. `broadside persona update NAME --field "Website=https://..." --field "GitHub=https://..."` sets metadata fields that appear on your Mastodon profile page.

**Q: Can we schedule posts?**

A: Not directly. Use cron, your CI system, or any scheduler to call the CLI or webhook at the desired time. Broadside publishes immediately on receipt.

**Q: What happens if we stop running broadside?**

A: Followers see no new posts. Their clients do not error. If the server is unreachable for an extended period, some remote servers will eventually unfollow (Mastodon drops followers after repeated delivery failures). Restarting broadside resumes normal operation. Followers do not need to re-follow.

---

### Technical FAQs

**Q: Why not just run Mastodon and only use it for posting?**

A: You can, but you are deploying and maintaining PostgreSQL, Redis, Sidekiq, and a Rails application to use 5% of its features. You also expose the full Mastodon attack surface — OAuth, client API, moderation endpoints, streaming, admin UI — none of which you need. Broadside's attack surface is deliberately minimal: a signed inbox, an authenticated webhook, and read-only discovery endpoints.

**Q: Why not use a Mastodon bot on someone else's instance?**

A: Your identity is `@bot@someone-elses-server.example`. You don't control the domain. The instance admin can suspend you. You're consuming resources on infrastructure meant for humans. For a corporate presence, you want `@name@corp.example` on infrastructure you control.

**Q: What's the delivery model?**

A: Activities are queued in a SQLite table and processed by an in-process tokio task. Retry schedule: 1m, 5m, 30m, 2h, 8h, then dead-letter. Per-domain circuit breaker pauses delivery after ten consecutive failures to prevent hammering broken servers. `broadside queue inspect` and `broadside queue retry` provide operator visibility.

**Q: How does RSS/Atom ingestion work?**

A: Broadside polls configured feeds on an interval (default 15 minutes). New entries are converted to ActivityPub Notes: title becomes the first line (bold), body is sanitized HTML, enclosure images become media attachments with blurhash, and links are appended. Deduplication is handled via the entry's `<id>` or `<guid>` stored as a source reference. Content is capped at 5000 characters with a "read more" link.

**Q: What about media?**

A: Images only (PNG, JPEG, GIF, WebP). CLI accepts local file paths via `--media`. Webhook accepts URLs (broadside fetches and caches). RSS entries use enclosure images. All images are validated by magic-byte MIME sniffing (not extension trust), capped at 10 MB, limited to 64 MB decoded (decompression bomb protection), stripped of EXIF data, and stored with computed blurhash for placeholder rendering.

**Q: How is security handled?**

A: All inbound inbox requests require HTTP Signature verification (fail-closed). Digest and Date headers are required and verified (prevents body substitution and replay attacks). The actor's public key is fetched, cached with 24-hour TTL, and validated for ownership. All outbound fetches are HTTPS-only with no redirect following, and private/link-local/metadata IP addresses are blocked at every fetch point. Webhook keys are compared using constant-time SHA-256 hash comparison. Content is sanitized via ammonia with a Mastodon-compatible HTML allowlist.

**Q: How do I back up my data?**

A: Copy the SQLite file and the `media/` directory. That is the entire instance state.

**Q: What about monitoring?**

A: `broadside status` reports personas, follower counts, queue depth, and failed deliveries. `GET /health` returns a JSON health endpoint suitable for external monitoring. The server warns at startup if the database or config file has insecure permissions.

---

### Business / Strategic FAQs

**Q: Is there a market for this?**

A: The fediverse is growing and organizations are starting to ask how to participate. The current answers are "run Mastodon" (too heavy) or "get a bot account" (don't own your identity). Broadside is the answer for organizations that want presence without participation. The market is small today but grows with fediverse adoption.

**Q: Could this be a hosted service?**

A: Yes, and more naturally than a full Mastodon hosting service. Each customer gets a broadside instance on their domain. The operator provisions a binary, a SQLite file, and a reverse proxy vhost. Per-customer resource consumption is minimal (under 50 MB RAM, near-zero CPU when idle). A managed "fediverse presence as a service" offering is a plausible business model.

**Q: What's the competitive landscape?**

A: Nothing does exactly this. Mastodon, GoToSocial, Pleroma, and Misskey are all interactive social servers. Honk and snac are minimal but still assume a human user. WordPress plugins (ActivityPub for WordPress) federate blog posts but require WordPress. Broadside is the first purpose-built broadcast-only ActivityPub server.

**Q: What's v2?**

A: Candidates: analytics (follower growth, reach estimates per post), post scheduling built in, Delete activity support (retract posts), multi-language posts (ActivityPub `contentMap`), and image alt-text prompting in the CLI. All are optional and none change the core architecture.

---

> **What is a PRFAQ?** A PRFAQ (Press Release / FAQ) is an Amazon-originated product planning technique. It starts with a fictional press release written as if the product has already launched successfully, forcing clarity on customer benefit and desired outcome. The FAQ section then anticipates hard internal and external questions. Writing the press release first ensures the team aligns on what success looks like before committing to implementation.
